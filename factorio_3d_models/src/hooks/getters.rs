// direct calls into the game's own getter functions (never hooked).
//
// msvc returns class types with user ctors through a HIDDEN POINTER in rdx
// (passing none crashes — the callee writes through garbage). a small pod
// would come back in rax instead, so both are captured and a sentinel in
// the buffer tells which convention the game actually used.

use crate::hooks::mem;
use crate::models::EntityModel;
use crate::offsets;
use crate::symbols::SymbolMap;
use crate::util::memo;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

// Entity::getPrototype() -> EntityPrototype*
type FnGetPrototype = unsafe extern "C" fn(*const core::ffi::c_void) -> *const u8;
// EntityWithHealth::getDeconstructionMarkerPosition() -> MapPosition (see top)
type FnGetPos = unsafe extern "C" fn(*const core::ffi::c_void, *mut crate::hooks::MapPos) -> u64;
// getOrientation / getDirection -> RealOrientation / Direction, same dance
type FnGetOrient = unsafe extern "C" fn(*const core::ffi::c_void, *mut u64) -> u64;
// Entity::getSurface() -> Surface& (plain pointer in rax)
type FnGetSurface = unsafe extern "C" fn(*const core::ffi::c_void) -> usize;

// resolved addresses (0 = unresolved, every wrapper checks)
static GET_PROTOTYPE: AtomicUsize = AtomicUsize::new(0);
static GET_POS: AtomicUsize = AtomicUsize::new(0);
static GET_SURFACE: AtomicUsize = AtomicUsize::new(0);
static TURRET_ORIENT: AtomicUsize = AtomicUsize::new(0); // Turret::getOrientation
static BASE_DIR: AtomicUsize = AtomicUsize::new(0); // Entity::getDirection
static BASE_ORIENT: AtomicUsize = AtomicUsize::new(0); // Entity::getOrientation

pub fn install(symbols: &SymbolMap, base: usize) {
    let resolve = |gf| super::resolve(symbols, base, gf);
    GET_PROTOTYPE.store(resolve(&offsets::ENTITY_GET_PROTOTYPE), Ordering::Relaxed);
    GET_POS.store(resolve(&offsets::EWH_DECON_MARKER_POS), Ordering::Relaxed);
    GET_SURFACE.store(resolve(&offsets::ENTITY_GET_SURFACE), Ordering::Relaxed);
    TURRET_ORIENT.store(resolve(&offsets::TURRET_GET_ORIENTATION), Ordering::Relaxed);
    BASE_DIR.store(resolve(&offsets::ENTITY_GET_DIRECTION), Ordering::Relaxed);
    BASE_ORIENT.store(resolve(&offsets::ENTITY_GET_ORIENTATION), Ordering::Relaxed);
}

// the class-override fallbacks for the generic draw table
pub fn base_dir_addr() -> usize {
    BASE_DIR.load(Ordering::Relaxed)
}
pub fn base_orient_addr() -> usize {
    BASE_ORIENT.load(Ordering::Relaxed)
}
pub fn turret_orient_addr() -> usize {
    TURRET_ORIENT.load(Ordering::Relaxed)
}

// EntityPrototype* of an entity (0 when unresolved / null entity)
pub fn prototype_of(this: *mut core::ffi::c_void) -> usize {
    let addr = GET_PROTOTYPE.load(Ordering::Relaxed);
    if addr == 0 || this.is_null() {
        return 0;
    }
    let get_proto: FnGetPrototype = unsafe { std::mem::transmute(addr) };
    unsafe { get_proto(this) as usize }
}

// prototype pointer -> model (one string scan per prototype, cached)
static PROTO_CACHE: LazyLock<Mutex<HashMap<usize, Option<EntityModel>>>> =
    LazyLock::new(Default::default);
static PROTOS_LOGGED: AtomicUsize = AtomicUsize::new(0);

fn proto_model(proto: usize) -> Option<EntityModel> {
    memo(&PROTO_CACHE, proto, || {
        // the name is the string at the lowest offset that resolves to a model
        let strings = mem::scan_proto_strings(proto as *const u8);
        let model = strings.iter().find_map(|(_, s)| crate::models::resolve(s));
        if PROTOS_LOGGED.fetch_add(1, Ordering::Relaxed) < 16 {
            log::info!(
                "[machines] prototype @0x{proto:X}: strings {strings:?} -> model={}",
                model.is_some()
            );
        }
        model
    })
}

// the 3d model replacing this entity, if any
pub fn entity_model(this: *mut core::ffi::c_void) -> Option<EntityModel> {
    let proto = prototype_of(this);
    if proto == 0 { None } else { proto_model(proto) }
}

// the 3d model for a raw EntityPrototype* (build-cursor placement preview,
// which hands us the prototype directly instead of an entity)
pub fn model_for_prototype(proto: usize) -> Option<EntityModel> {
    if proto == 0 { None } else { proto_model(proto) }
}

// Surface* the entity lives on (0 when the getter isn't resolved)
pub fn entity_surface(this: *mut core::ffi::c_void) -> usize {
    let addr = GET_SURFACE.load(Ordering::Relaxed);
    if addr == 0 || this.is_null() {
        return 0;
    }
    let get_surface: FnGetSurface = unsafe { std::mem::transmute(addr) };
    unsafe { get_surface(this) }
}

// entity map position in tiles via the marker-position getter (None when
// unresolved). only safe on EntityWithHealth-derived classes
pub fn entity_pos(this: *mut core::ffi::c_void) -> Option<(f32, f32)> {
    let addr = GET_POS.load(Ordering::Relaxed);
    if addr == 0 {
        return None;
    }
    let get_pos: FnGetPos = unsafe { std::mem::transmute(addr) };
    let mut buf = crate::hooks::MapPos { x: i32::MIN, y: i32::MIN };
    let rax = unsafe { get_pos(this, &mut buf) };
    let (px, py) = if buf.x != i32::MIN || buf.y != i32::MIN {
        (buf.x, buf.y)
    } else {
        ((rax & 0xFFFF_FFFF) as u32 as i32, (rax >> 32) as u32 as i32)
    };
    Some((px as f32 / offsets::RECT_FP, py as f32 / offsets::RECT_FP))
}

// entity map position straight from the base field (safe on ANY entity —
// resources aren't EntityWithHealth, the getter above would be wrong)
pub fn entity_pos_field(this: *mut core::ffi::c_void) -> Option<(f32, f32)> {
    if this.is_null() {
        return None;
    }
    let p: crate::hooks::MapPos = mem::try_read(this as usize + offsets::ENTITY_POS_FIELD)?;
    Some((p.x as f32 / offsets::RECT_FP, p.y as f32 / offsets::RECT_FP))
}

// building direction 0..16 via the class's own getDirection override
pub fn entity_dir(this: *mut core::ffi::c_void, addr: usize) -> u8 {
    if addr == 0 {
        return 0;
    }
    let get_dir: FnGetOrient = unsafe { std::mem::transmute(addr) };
    let mut buf = u64::MAX;
    let rax = unsafe { get_dir(this, &mut buf) };
    for raw in [buf, rax] {
        let d = (raw & 0xFF) as u8;
        if d < 16 {
            return d;
        }
    }
    0
}

// smooth 0..1 orientation via the given getter address (NAN when unresolved)
pub fn entity_orientation(this: *mut core::ffi::c_void, addr: usize) -> f32 {
    if addr == 0 {
        return f32::NAN;
    }
    let get_orient: FnGetOrient = unsafe { std::mem::transmute(addr) };
    let mut buf = u64::MAX; // NaN as f32/f64 -> fails the 0..1 check
    let rax = unsafe { get_orient(this, &mut buf) };
    decode_orientation(buf, rax)
}

// the wrapped value may be a float or a double, via buffer or rax — accept
// whichever decodes to a sane 0..1 (buffer first: hidden-pointer is the
// likely convention for a class with ctors)
fn decode_orientation(buf: u64, rax: u64) -> f32 {
    for raw in [buf, rax] {
        let as_f64 = f64::from_bits(raw);
        if (0.0..=1.0).contains(&as_f64) {
            return as_f64 as f32;
        }
        let as_f32 = f32::from_bits((raw & 0xFFFF_FFFF) as u32);
        if (0.0..=1.0).contains(&as_f32) {
            return as_f32;
        }
    }
    0.0
}

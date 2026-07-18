// entity draw hooks: sprite suppression + data feed for the 3d model replacement.
//
// how it works:
// - the ::draw virtual of each supported entity class is bracketed: the
//   entity's prototype name is resolved against the model registry (one
//   string scan per prototype) and marked on the thread while it draws.
// - WorkingVisualisations::drawCraftingMachine / drawMiningDrill draw the
//   machine's sprites and receive the animation state, status, map position
//   and direction. inside a bracket they record all of that into the
//   registry and RETURN WITHOUT drawing — the 3d model takes over.
// - turrets have no such inner call with a position, so Turret::draw itself
//   is suppressed; position comes from getDeconstructionMarkerPosition and
//   the head rotation from Turret::getOrientation.
// - ~Entity (base class, end of every destruction chain) removes the entity
//   from the registry, so mined / destroyed entities vanish and pointers
//   never dangle.
//
// entity draws only run when a chunk re-queues; a working machine's
// animation advances every tick which re-queues its chunk every frame, so
// the recorded fingerprint changing = the machine is working.

use crate::entities::Record;
use crate::hooks::mem::{self, dq_is_ghost};
use crate::hooks::{MapPos, getters, hook, items, wires};
use crate::models::EntityModel;
use crate::offsets;
use crate::symbols::SymbolMap;
use crate::util::memo;
use anyhow::Result;
use retour::static_detour;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

type FnEntityDraw = unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
// Car::getRelativeTurretOrientation() -> float (plain M return, xmm0)
type FnGetTurret = unsafe extern "C" fn(*const core::ffi::c_void) -> f32;
// Inserter::getActivityProgress() -> double (plain N return, xmm0)
type FnGetF64 = unsafe extern "C" fn(*const core::ffi::c_void) -> f64;

static_detour! {
    static AsmDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static FurnaceDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static DrillDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static TurretDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static AmmoTurretDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static EntityDtorHook: unsafe extern "C" fn(*mut core::ffi::c_void);
    static DieHook: unsafe extern "C" fn(*mut core::ffi::c_void, usize, usize, usize);
    static RocketDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static RocketCroppedHook: unsafe extern "C" fn(
        usize, usize, usize, usize, usize, usize, usize, usize, usize, usize);
    // BuildingRenderer::drawEntityToBeBuilt — the build-cursor placement preview
    static DrawE2BHook: unsafe extern "C" fn(
        usize, usize, usize, usize, usize, usize, usize, usize, usize) -> usize;
    static EntityBtnAHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static EntityBtnBHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    // WorkingVisualisations::drawCraftingMachine(DrawQueue&, AnimState const&,
    //   uchar, Status, bool, bool, bool, Surface const*, MapPosition const&,
    //   Direction, EnergySource const*, RecipeTints const*, EnabledIndices)
    //   — integer-class args, forwarded as usize
    static DrawCmHook: unsafe extern "C" fn(
        usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize);
    // WorkingVisualisations::drawMiningDrill(DrawQueue&, GraphicsSet const&,
    //   AnimState const&, MapTick, bool, bool, Status, bool, Surface const*,
    //   MapPosition const&, Direction, EnergySource const*, FluidPrototype const*)
    static DrawMdHook: unsafe extern "C" fn(
        usize, usize, usize, usize, usize, usize, usize,
        usize, usize, usize, usize, usize, usize, usize);
}

thread_local! {
    // entity currently drawing on this thread + its model
    static CURRENT: std::cell::RefCell<Option<(usize, EntityModel)>> =
        const { std::cell::RefCell::new(None) };
    // set while a GUI entity-preview button draws an entity (silo menu etc.) —
    // our hooks pass through so previews keep their vanilla 2d look
    static IN_GUI_PREVIEW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static IN_ROCKET_DRAW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn in_gui_preview() -> bool {
    IN_GUI_PREVIEW.with(|c| c.get())
}

// this inserter tier's rotation_speed (f64 at InserterPrototype +0x6E0),
// cached per prototype pointer
static INSERTER_ROT_CACHE: LazyLock<Mutex<HashMap<usize, f32>>> = LazyLock::new(Default::default);

fn inserter_rotation_speed(this: *mut core::ffi::c_void) -> Option<f32> {
    let proto = getters::prototype_of(this);
    if proto == 0 {
        return None;
    }
    Some(memo(&INSERTER_ROT_CACHE, proto, || {
        let rs = mem::try_read::<f64>(proto + offsets::INSERTER_ROTATION_SPEED)
            .map(|v| v as f32)
            .unwrap_or(0.0);
        if rs.is_finite() && rs > 0.0001 && rs < 1.0 { rs } else { offsets::INSERTER_BASE_ROTATION }
    }))
}

// --- bracket hooks: mark the drawing entity, the WV hooks do the rest -----------------

fn bracketed_draw(
    this: *mut core::ffi::c_void,
    queue: *mut core::ffi::c_void,
    call: impl FnOnce(*mut core::ffi::c_void, *mut core::ffi::c_void),
) {
    let model = if this.is_null() { None } else { getters::entity_model(this) };
    if let Some(m) = model {
        CURRENT.with(|c| *c.borrow_mut() = Some((this as usize, m)));
        call(this, queue);
        CURRENT.with(|c| *c.borrow_mut() = None);
    } else {
        call(this, queue);
    }
}

fn hooked_asm_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    bracketed_draw(this, queue, |t, q| unsafe { AsmDrawHook.call(t, q) });
}

fn hooked_furnace_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    bracketed_draw(this, queue, |t, q| unsafe { FurnaceDrawHook.call(t, q) });
}

fn hooked_drill_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    bracketed_draw(this, queue, |t, q| unsafe { DrillDrawHook.call(t, q) });
}

// --- the shared record-and-suppress core -----------------------------------------------

// crafting-machine working signal (layout: disasm of AnimState::update @ rva
// 0x3AE740). the last-update tick changes on EVERY tick while the machine
// animates and freezes when it stops
fn anim_fingerprint(anim: usize) -> u64 {
    if anim == 0 {
        return 0;
    }
    let tick = mem::read::<u64>(anim + offsets::CM_ANIM_TICK);
    let frame = mem::read::<u16>(anim + offsets::CM_ANIM_FRAME);
    tick ^ ((frame as u64) << 48)
}

// mining-drill / pumpjack working signal. the drawMiningDrill AnimState is a
// DIFFERENT struct from the crafting machine's: its +0x04 frame / +0x10 tick
// stay constant here, which is why drills never animated. verified in-game
// (pumping pumpjack): the low dwords at +0x00 and +0x08 climb ~1/frame while
// the drill works and freeze when it idles — that's the real fingerprint.
fn mining_drill_fp(anim: usize) -> u64 {
    if anim == 0 || !mem::readable(anim as *const u8, 0x10) {
        return 0;
    }
    let a = mem::read::<u32>(anim + offsets::MD_ANIM_LO) as u64;
    let b = mem::read::<u32>(anim + offsets::MD_ANIM_HI) as u64;
    a | (b << 32)
}

// record the bracketed entity; true = swallow the vanilla sprites.
// `fingerprint` changes each tick while the machine works
fn record_current(dq: usize, fingerprint: u64, pos: usize, dir: usize, surface: usize) -> bool {
    if in_gui_preview() {
        return false; // GUI preview keeps its vanilla sprite
    }
    let Some((entity, model)) = CURRENT.with(|c| c.borrow().clone()) else { return false };
    if entity == 0 || pos == 0 {
        return false;
    }
    // ghost mode = a blueprint/dead-building ghost of this machine (or the
    // build-cursor preview). keep the vanilla sprite, add a transparent 3d
    // ghost on top so destroyed/ghost crafting machines & drills show in 3d too
    if dq_is_ghost(dq) {
        let (x, y) = unsafe { MapPos::tiles_at(pos) };
        crate::entities::record_ghost(&model, x, y, (dir & 0xFF) as u8);
        return false;
    }
    let (x, y) = unsafe { MapPos::tiles_at(pos) };
    crate::entities::record(Record {
        dir: (dir & 0xFF) as u8,
        surface,
        fingerprint,
        ..Record::at(entity, &model, x, y)
    })
    // only established machines lose their sprite — a brand-new entry is
    // either a just-placed machine (vanilla for ~2 frames, imperceptible)
    // or the cursor-preview temp, whose sprite must keep drawing
}

#[allow(clippy::too_many_arguments)]
fn hooked_draw_cm(
    this: usize, dq: usize, anim: usize, e: usize, status: usize, b1: usize, b2: usize,
    b3: usize, surface: usize, pos: usize, dir: usize, energy: usize, tints: usize, wvei: usize,
) {
    if record_current(dq, anim_fingerprint(anim), pos, dir, surface) {
        return;
    }
    unsafe {
        DrawCmHook.call(this, dq, anim, e, status, b1, b2, b3, surface, pos, dir, energy, tints, wvei)
    }
}

#[allow(clippy::too_many_arguments)]
fn hooked_draw_md(
    this: usize, dq: usize, gs: usize, anim: usize, tick: usize, b1: usize, b2: usize,
    status: usize, b3: usize, surface: usize, pos: usize, dir: usize, energy: usize, fluid: usize,
) {
    if record_current(dq, mining_drill_fp(anim), pos, dir, surface) {
        return;
    }
    unsafe {
        DrawMdHook.call(this, dq, gs, anim, tick, b1, b2, status, b3, surface, pos, dir, energy, fluid)
    }
}

// --- generic buildings & vehicles: suppress the whole draw -----------------------------

struct GenericHook {
    det: retour::GenericDetour<FnEntityDraw>,
    oriented: bool,
    anim_on_draw: bool,   // play the glb anim while the chunk redraws
    dir_addr: usize,      // the class's getDirection (or the Entity base)
    orient_addr: usize,   // the class's getOrientation, oriented targets only
    turret_addr: usize,   // getRelativeTurretOrientation (tank), 0 = none
    progress_addr: usize, // getActivityProgress (inserters), 0 = none
    wire_off: usize,      // WireConnector offset (poles), 0 = none
    activity_addr: usize, // getActivityRate (accumulator), 0 = none
    gate_off: usize,      // gate openingProgress field offset, 0 = none
}

static GENERIC_DETOURS: std::sync::OnceLock<Vec<GenericHook>> = std::sync::OnceLock::new();

fn generic_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void, idx: usize) {
    let Some(detours) = GENERIC_DETOURS.get() else { return };
    let h = &detours[idx];
    let call = |t, q| unsafe { h.det.call(t, q) };

    let model = if this.is_null() { None } else { getters::entity_model(this) };
    let Some(model) = model else {
        call(this, queue);
        return;
    };
    if dq_is_ghost(queue as usize) || in_gui_preview() {
        // ghost entities (blueprint/dead-building ghosts, hover previews) draw
        // in ghost mode — keep the vanilla sprite and add a transparent 3d
        // model on top. GUI previews stay 2d
        if dq_is_ghost(queue as usize) && !in_gui_preview() {
            if let Some((x, y)) = getters::entity_pos_field(this) {
                crate::entities::record_ghost(&model, x, y, getters::entity_dir(this, h.dir_addr));
            }
        }
        call(this, queue);
        return;
    }
    // the base position field is safe on any entity (resources aren't
    // EntityWithHealth, so the marker-position getter would be wrong)
    let Some((x, y)) = getters::entity_pos_field(this) else {
        call(this, queue);
        return;
    };
    let dir = getters::entity_dir(this, h.dir_addr);
    // gate: the live opening progress (1 closed .. 0 open). its bits are the
    // fingerprint (changing = the gate is moving) and its inverse the morph
    let gate_progress = (h.gate_off != 0)
        .then(|| mem::try_read::<f32>(this as usize + h.gate_off))
        .flatten()
        .filter(|p| p.is_finite());
    let mut turret = f32::NAN;
    let (orientation, fingerprint) = if h.oriented {
        let o = getters::entity_orientation(this, h.orient_addr);
        if h.turret_addr != 0 {
            let get_turret: FnGetTurret = unsafe { std::mem::transmute(h.turret_addr) };
            let t = unsafe { get_turret(this) };
            if (0.0..=1.0).contains(&t) {
                turret = t;
            }
        }
        // moving = position/orientation changing = play the animation
        let fp = (x.to_bits() as u64) << 32 ^ y.to_bits() as u64 ^ (o.to_bits() as u64) << 16;
        (o, fp)
    } else if let Some(p) = gate_progress {
        (f32::NAN, p.to_bits() as u64)
    } else if h.progress_addr != 0 {
        // inserters: the activity progress advances exactly while the arm
        // swings — changing progress = play the animation
        let get_progress: FnGetF64 = unsafe { std::mem::transmute(h.progress_addr) };
        let p = unsafe { get_progress(this) };
        (f32::NAN, p.to_bits())
    } else if h.anim_on_draw {
        // being redrawn at all = something animates here = swing the arm
        (f32::NAN, crate::hooks::frame::frame_count())
    } else {
        (f32::NAN, 0)
    };
    let suppress = crate::entities::record(Record {
        dir,
        orientation,
        turret,
        surface: getters::entity_surface(this),
        fingerprint,
        ..Record::at(this as usize, &model, x, y)
    });
    // gate: shapekey weight = 1 - openingProgress (shapekey 0 = extended/
    // closed, 1 = retracted into the floor = open)
    if let Some(p) = gate_progress {
        crate::entities::set_morph(this as usize, (1.0 - p).clamp(0.0, 1.0));
    }
    // activity rate (accumulator): a nonzero value means the entity is
    // actively working right now — gates its active-only prims (the arcs)
    if h.activity_addr != 0 {
        let get_rate: FnGetF64 = unsafe { std::mem::transmute(h.activity_addr) };
        let rate = unsafe { get_rate(this) };
        crate::entities::set_active(this as usize, rate.is_finite() && rate.abs() > 1e-6);
    }
    // inserters: scale the arm animation to this tier's real rotation_speed,
    // feed the live arm angle so the model arm tracks the real one exactly, and
    // record the held item as a lifted 3d model at the reconstructed hand
    if suppress && h.progress_addr != 0 {
        if let Some(rs) = inserter_rotation_speed(this) {
            crate::entities::set_anim_rate(this as usize, rs / offsets::INSERTER_BASE_ROTATION);
        }
        let mut arm = f32::NAN;
        if let Some(a) = mem::try_read::<f32>(this as usize + offsets::INSERTER_ARM_ANGLE) {
            if a.is_finite() {
                arm = a.rem_euclid(1.0);
                crate::entities::set_arm_angle(this as usize, arm);
            }
        }
        if !dq_is_ghost(queue as usize) {
            items::record_inserter_hand(this, x, y, if arm.is_finite() { arm } else { 0.0 });
        }
    }
    if !suppress {
        call(this, queue);
    } else if h.wire_off != 0 {
        wires::submit_pole_wires(queue as usize, this as usize, h.wire_off, x, y);
    }
}

// retour needs a distinct detour fn per target: one thin shim per table slot
macro_rules! shims {
    ($($i:expr => $n:ident),* $(,)?) => {
        $( unsafe extern "C" fn $n(t: *mut core::ffi::c_void, q: *mut core::ffi::c_void) {
            generic_draw(t, q, $i)
        } )*
        const GENERIC_SHIMS: &[FnEntityDraw] = &[ $($n),* ];
    };
}
shims!(
    0 => g0, 1 => g1, 2 => g2, 3 => g3, 4 => g4, 5 => g5, 6 => g6, 7 => g7,
    8 => g8, 9 => g9, 10 => g10, 11 => g11, 12 => g12, 13 => g13, 14 => g14,
    15 => g15, 16 => g16, 17 => g17, 18 => g18, 19 => g19, 20 => g20, 21 => g21,
    22 => g22, 23 => g23, 24 => g24, 25 => g25, 26 => g26, 27 => g27, 28 => g28,
    29 => g29, 30 => g30, 31 => g31, 32 => g32, 33 => g33, 34 => g34, 35 => g35,
    36 => g36, 37 => g37, 38 => g38, 39 => g39, 40 => g40, 41 => g41, 42 => g42,
    43 => g43,
);

// --- turrets: suppress the whole draw, data comes from getters --------------------------

fn turret_draw_common(
    this: *mut core::ffi::c_void,
    queue: *mut core::ffi::c_void,
    call: impl FnOnce(*mut core::ffi::c_void, *mut core::ffi::c_void),
) {
    let model = if this.is_null() { None } else { getters::entity_model(this) };
    let Some(model) = model else {
        call(this, queue);
        return;
    };
    if dq_is_ghost(queue as usize) || in_gui_preview() {
        if dq_is_ghost(queue as usize) && !in_gui_preview() {
            if let Some((x, y)) = getters::entity_pos_field(this) {
                crate::entities::record_ghost(&model, x, y, 0);
            }
        }
        call(this, queue);
        return;
    }
    let Some((x, y)) = getters::entity_pos(this) else {
        call(this, queue);
        return;
    };
    let orientation = getters::entity_orientation(this, getters::turret_orient_addr());
    let suppress = crate::entities::record(Record {
        orientation,
        surface: getters::entity_surface(this),
        fingerprint: orientation.to_bits() as u64, // rotating = working
        ..Record::at(this as usize, &model, x, y)
    });
    if !suppress {
        call(this, queue);
    }
}

fn hooked_turret_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    turret_draw_common(this, queue, |t, q| unsafe { TurretDrawHook.call(t, q) });
}

fn hooked_ammo_turret_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    turret_draw_common(this, queue, |t, q| unsafe { AmmoTurretDrawHook.call(t, q) });
}

// --- rocket silo rocket: 3d rocket rising out of the silo -------------------------------
// the RocketSiloRocket entity exists exactly while a rocket is loaded. its
// draw calls drawRocketCroppedSprite with a rise offset; we suppress those
// vanilla sprites, capture the rise, and record a 3d rocket lifted by it

fn hooked_rocket_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    let model = if this.is_null() { None } else { getters::entity_model(this) };
    let Some(model) = model else {
        unsafe { RocketDrawHook.call(this, queue) };
        return;
    };
    // GUI silo-menu preview: keep the vanilla 2d rocket
    if dq_is_ghost(queue as usize) || in_gui_preview() {
        unsafe { RocketDrawHook.call(this, queue) };
        return;
    }
    let Some((x, y)) = getters::entity_pos_field(this) else {
        unsafe { RocketDrawHook.call(this, queue) };
        return;
    };
    // run the original with the cropped-sprite calls suppressed
    IN_ROCKET_DRAW.with(|c| c.set(true));
    unsafe { RocketDrawHook.call(this, queue) };
    IN_ROCKET_DRAW.with(|c| c.set(false));
    let _ = crate::entities::record(Record {
        surface: getters::entity_surface(this),
        ..Record::at(this as usize, &model, x, y)
    });
    // flight 0: the launch altitude field isn't identified, so only the
    // emerge animation runs. the tag still marks this entity as the rocket
    crate::entities::set_rocket(this as usize, 0.0);
}

#[allow(clippy::too_many_arguments)]
fn hooked_rocket_cropped(
    dq: usize, sprite: usize, orient: usize, offset: usize, a5: usize, a6: usize,
    a7: usize, a8: usize, a9: usize, a10: usize,
) {
    if IN_ROCKET_DRAW.with(|c| c.get()) {
        return; // swallow the vanilla rocket sprite
    }
    unsafe { RocketCroppedHook.call(dq, sprite, orient, offset, a5, a6, a7, a8, a9, a10) }
}

// the build-cursor placement preview. arg1 = GameView&, arg5 = build
// MapPosition&, arg6 = EntityPrototype* being placed. record a transparent 3d
// ghost of it, then let the vanilla 2d green preview draw underneath (so both
// show while placing)
#[allow(clippy::too_many_arguments)]
fn hooked_draw_e2b(
    a1: usize, a2: usize, ghost: usize, mode: usize, pos: usize,
    proto: usize, ctrl: usize, id: usize, dq: usize,
) -> usize {
    if !in_gui_preview() && pos != 0 && proto != 0 {
        if let Some(model) = getters::model_for_prototype(proto) {
            let (x, y) = unsafe { MapPos::tiles_at(pos) };
            // snap to the build grid like the game does: an odd-tile footprint
            // centers on a tile center (x.5), an even one on a tile corner (x.0)
            let snap = |v: f32| {
                if model.tiles.round() as i32 % 2 == 1 {
                    (v - 0.5).round() + 0.5
                } else {
                    v.round()
                }
            };
            // the cursor's R-rotation, exactly like the game reads it inside
            // this function: an override direction wins when its flag is set,
            // otherwise the GameView field
            let dir = (|| {
                let a = mem::try_read::<usize>(a1 + offsets::GAMEVIEW_DIR_CHAIN_A)?;
                let b = mem::try_read::<usize>(a + offsets::GAMEVIEW_DIR_CHAIN_B)?;
                if mem::try_read::<u8>(b + offsets::DIR_OVERRIDE_FLAG)? != 0 {
                    mem::try_read::<u8>(b + offsets::DIR_OVERRIDE_VALUE)
                } else {
                    mem::try_read::<u8>(a1 + offsets::GAMEVIEW_BUILD_DIR)
                }
            })()
            .map(|d| d & 0xF)
            .unwrap_or(0);
            crate::entities::record_ghost(&model, snap(x), snap(y), dir);
        }
    }
    unsafe { DrawE2BHook.call(a1, a2, ghost, mode, pos, proto, ctrl, id, dq) }
}

// --- gui previews / destruction ----------------------------------------------------------

// GUI entity-preview buttons: bracket so nested entity draws pass through
fn hooked_entity_btn_a(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    IN_GUI_PREVIEW.with(|c| c.set(true));
    unsafe { EntityBtnAHook.call(this, queue) };
    IN_GUI_PREVIEW.with(|c| c.set(false));
}

fn hooked_entity_btn_b(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    IN_GUI_PREVIEW.with(|c| c.set(true));
    unsafe { EntityBtnBHook.call(this, queue) };
    IN_GUI_PREVIEW.with(|c| c.set(false));
}

// destroyed buildings drop the model the moment they die (remnants take over)
fn hooked_die(this: *mut core::ffi::c_void, force: usize, cause: usize, dmg: usize) {
    crate::entities::remove(this as usize);
    unsafe { DieHook.call(this, force, cause, dmg) }
}

// every entity type funnels through ~Entity
fn hooked_entity_dtor(this: *mut core::ffi::c_void) {
    crate::entities::remove(this as usize);
    unsafe { EntityDtorHook.call(this) }
}

// --- install ------------------------------------------------------------------------------

// run `f` with every other thread of this process suspended.
//
// enabling a detour is a non-atomic multi-byte write over live code, and
// these targets are hot on the game's update/prepare threads — patching them
// while a thread runs mid-prologue crashes. inside `f`: no allocation, no
// logging, no locks (a suspended thread frozen inside malloc would deadlock)
fn with_other_threads_suspended<R>(f: impl FnOnce() -> R) -> R {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
        THREAD_SUSPEND_RESUME,
    };
    let me = unsafe { GetCurrentThreadId() };
    let pid = unsafe { GetCurrentProcessId() };
    // pre-allocate BEFORE suspending anything
    let mut suspended = Vec::with_capacity(256);
    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            let mut te = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            if Thread32First(snap, &mut te).is_ok() {
                loop {
                    if te.th32OwnerProcessID == pid && te.th32ThreadID != me {
                        if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, te.th32ThreadID) {
                            if suspended.len() < suspended.capacity() {
                                SuspendThread(h);
                                suspended.push(h);
                            } else {
                                let _ = CloseHandle(h);
                            }
                        }
                    }
                    te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
                    if Thread32Next(snap, &mut te).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
    }
    let r = f();
    unsafe {
        for h in suspended {
            ResumeThread(h);
            let _ = CloseHandle(h);
        }
    }
    r
}

pub fn install(symbols: &SymbolMap, base: usize) -> Result<()> {
    getters::install(symbols, base);

    // phase 1: initialize (allocates trampolines) — nothing observable yet
    unsafe {
        hook!(symbols, base, AsmDrawHook, ASSEMBLING_MACHINE_DRAW, hooked_asm_draw);
        hook!(symbols, base, FurnaceDrawHook, FURNACE_DRAW, hooked_furnace_draw);
        hook!(symbols, base, DrillDrawHook, MINING_DRILL_DRAW, hooked_drill_draw);
        hook!(symbols, base, TurretDrawHook, TURRET_DRAW, hooked_turret_draw);
        hook!(symbols, base, AmmoTurretDrawHook, AMMO_TURRET_DRAW, hooked_ammo_turret_draw);
        hook!(symbols, base, EntityDtorHook, ENTITY_DTOR, hooked_entity_dtor);
        hook!(symbols, base, DieHook, EWH_DIE, hooked_die);
        hook!(symbols, base, DrawCmHook, WV_DRAW_CRAFTING_MACHINE, hooked_draw_cm);
        hook!(symbols, base, DrawMdHook, WV_DRAW_MINING_DRILL, hooked_draw_md);
        hook!(symbols, base, RocketDrawHook, ROCKET_DRAW, hooked_rocket_draw);
        hook!(symbols, base, RocketCroppedHook, ROCKET_CROPPED_SPRITE, hooked_rocket_cropped);
        hook!(symbols, base, DrawE2BHook, DRAW_ENTITY_TO_BE_BUILT, hooked_draw_e2b);
        hook!(symbols, base, EntityBtnAHook, ENTITY_BUTTON_DRAW_A, hooked_entity_btn_a);
        hook!(symbols, base, EntityBtnBHook, ENTITY_BUTTON_DRAW_B, hooked_entity_btn_b);
    }
    items::init(symbols, base)?;
    wires::init(symbols, base)?;
    super::daynight::init(symbols, base)?;
    super::player::init(symbols, base)?;

    // generic building/vehicle draws: one detour per table slot. dir/orient
    // getters resolve to the class's own override; when a class has none the
    // Entity base impl is correct (nothing overrides it away)
    let by_symbol = |sym: &str| -> Option<usize> {
        (!sym.is_empty())
            .then(|| symbols.iter().find(|(n, _)| n.contains(sym)).map(|(_, a)| *a))
            .flatten()
    };
    let mut generic: Vec<GenericHook> = Vec::new();
    for (i, target) in offsets::GENERIC_DRAWS.iter().enumerate() {
        if i >= GENERIC_SHIMS.len() {
            log::warn!("[machines] more generic draw targets than shims — extend shims!()");
            break;
        }
        let addr = super::resolve(symbols, base, &target.gf);
        let det = unsafe {
            let f: FnEntityDraw = std::mem::transmute(addr);
            retour::GenericDetour::new(f, GENERIC_SHIMS[i])?
        };
        generic.push(GenericHook {
            det,
            oriented: target.oriented,
            anim_on_draw: target.anim_on_draw,
            dir_addr: by_symbol(target.dir_sym).unwrap_or(getters::base_dir_addr()),
            orient_addr: by_symbol(target.orient_sym).unwrap_or(getters::base_orient_addr()),
            turret_addr: by_symbol(target.turret_sym).unwrap_or(0),
            progress_addr: by_symbol(target.progress_sym).unwrap_or(0),
            wire_off: target.wire_off,
            activity_addr: by_symbol(target.activity_sym).unwrap_or(0),
            gate_off: target.gate_off,
        });
    }
    let generic_count = generic.len();
    GENERIC_DETOURS.set(generic).map_err(|_| anyhow::anyhow!("generic detours already set"))?;

    // phase 2: write the patches while no other thread can be executing the
    // target prologues (draws run on prepare workers, the dtor on update)
    with_other_threads_suspended(|| -> Result<()> {
        unsafe {
            AsmDrawHook.enable()?;
            FurnaceDrawHook.enable()?;
            DrillDrawHook.enable()?;
            TurretDrawHook.enable()?;
            AmmoTurretDrawHook.enable()?;
            EntityDtorHook.enable()?;
            DieHook.enable()?;
            DrawCmHook.enable()?;
            DrawMdHook.enable()?;
            RocketDrawHook.enable()?;
            RocketCroppedHook.enable()?;
            DrawE2BHook.enable()?;
            EntityBtnAHook.enable()?;
            EntityBtnBHook.enable()?;
        }
        items::enable()?;
        wires::enable()?;
        super::player::enable()?;
        if let Some(generic) = GENERIC_DETOURS.get() {
            for h in generic {
                unsafe { h.det.enable()? };
            }
        }
        Ok(())
    })?;
    log::info!(
        "entity hooks installed (crafting machines, furnaces, drills, turrets, {generic_count} generic classes)"
    );
    Ok(())
}

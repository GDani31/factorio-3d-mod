// colored day/night sky + planet detection.
//
// the "sky" is the background the tilted canvas doesn't cover (the fbo clear
// color). we color it per planet and blend day->night by the game's darkness.
// - darkness comes from hooking DayTime::getDarkness (capture its return)
// - the planet comes from GameView::getSurface() + reading the surface's name

use crate::offsets;
use crate::symbols::SymbolMap;
use anyhow::Result;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering};

// darkness 0(day)..1(night), f32 bits — written by the naked shim below
static DARKNESS: AtomicU32 = AtomicU32::new(0);
// trampoline (original getDarkness) address for the shim to call
static DARKNESS_TRAMP: AtomicUsize = AtomicUsize::new(0);
// detected planet id (see settings::sky_day_night); 0 = nauvis default
static PLANET: AtomicU8 = AtomicU8::new(0);
// resolved GameView::getSurface address
static GET_SURFACE: AtomicUsize = AtomicUsize::new(0);
// last surface we classified, to skip re-scanning every frame
static LAST_SURFACE: AtomicUsize = AtomicUsize::new(0);

type FnGetSurface = unsafe extern "C" fn(*mut core::ffi::c_void) -> *mut core::ffi::c_void;

// DayTime::getDarkness is a small leaf function; its callers (e.g.
// EnemySpawner::spawnEnemies) keep live values in volatile registers across
// the call under whole-program optimization, so a normal detour corrupts them
// and crashes. this naked shim just calls the original and copies the float
// return (xmm0) into DARKNESS — it touches only rsp, so callers see exactly
// the original's register effects.
#[unsafe(naked)]
unsafe extern "C" fn darkness_shim() {
    core::arch::naked_asm!(
        "sub rsp, 0x28", // shadow space + 16-byte align for the call
        "call qword ptr [rip + {tramp}]",
        "movss dword ptr [rip + {slot}], xmm0",
        "add rsp, 0x28",
        "ret",
        tramp = sym DARKNESS_TRAMP,
        slot = sym DARKNESS,
    )
}

pub fn darkness() -> f32 {
    f32::from_bits(DARKNESS.load(Ordering::Relaxed))
}

pub fn planet() -> u8 {
    PLANET.load(Ordering::Relaxed)
}

// current sky color = planet day/night palette blended by darkness
pub fn color() -> [f32; 3] {
    let d = darkness().clamp(0.0, 1.0);
    let (day, night) = crate::settings::sky_day_night(planet());
    [
        day[0] + (night[0] - day[0]) * d,
        day[1] + (night[1] - day[1]) * d,
        day[2] + (night[2] - day[2]) * d,
    ]
}

pub fn install(symbols: &SymbolMap, base: usize) -> Result<()> {
    GET_SURFACE.store(
        crate::hooks::resolve(symbols, base, &offsets::GAME_VIEW_GET_SURFACE),
        Ordering::Relaxed,
    );

    // raw detour onto the register-preserving shim (never a normal detour —
    // this is a leaf function hot on the update thread)
    let addr = crate::hooks::resolve(symbols, base, &offsets::DAYTIME_GET_DARKNESS);
    let det = unsafe {
        retour::RawDetour::new(addr as *const (), darkness_shim as *const ())?
    };
    DARKNESS_TRAMP.store(det.trampoline() as *const _ as usize, Ordering::SeqCst);
    // enable with the other threads suspended so the patch isn't written under
    // a live execution of the prologue
    crate::hooks::rotation::with_other_threads_suspended(|| unsafe { det.enable() })?;
    std::mem::forget(det); // must live for the process lifetime
    log::info!("DayTime::getDarkness shim-hook installed (day/night sky)");
    Ok(())
}

// called from the main-view createRenderParameters hook (this = GameView)
pub fn detect_planet(game_view: *mut core::ffi::c_void) {
    if game_view.is_null() {
        return;
    }
    let gs = GET_SURFACE.load(Ordering::Relaxed);
    if gs == 0 {
        return;
    }
    let surface = unsafe {
        let f: FnGetSurface = std::mem::transmute(gs);
        f(game_view)
    };
    if surface.is_null() {
        return;
    }
    // same surface as last time -> keep the cached planet
    if LAST_SURFACE.swap(surface as usize, Ordering::Relaxed) == surface as usize {
        return;
    }
    if let Some(id) = classify_surface(surface) {
        PLANET.store(id, Ordering::Relaxed);
        log::info!("[sky] planet detected: id {id}");
    } else {
        log::info!("[sky] planet not detected — using nauvis default (set it in settings)");
    }
}

// read the surface object's bytes (crash-safe via ReadProcessMemory) and look
// for a known planet name — the main-planet surfaces are named after them
fn classify_surface(surface: *mut core::ffi::c_void) -> Option<u8> {
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut buf = vec![0u8; 0x2000];
    let mut read = 0usize;
    unsafe {
        // partial reads stop at the first unreadable page and report the
        // count, so this never faults on a short/edge object
        let _ = ReadProcessMemory(
            GetCurrentProcess(),
            surface as *const _,
            buf.as_mut_ptr() as *mut _,
            buf.len(),
            Some(&mut read),
        );
    }
    if read == 0 {
        return None;
    }
    buf.truncate(read);

    // distinctive names first; platform last so real planets win
    // (space platform surfaces are named "platform-N")
    const NAMES: [(&[u8], u8); 6] = [
        (b"vulcanus", 1),
        (b"fulgora", 2),
        (b"gleba", 3),
        (b"aquilo", 4),
        (b"nauvis", 0),
        (b"platform", 5),
    ];
    for (needle, id) in NAMES {
        if buf.windows(needle.len()).any(|w| w == needle) {
            return Some(id);
        }
    }
    None
}

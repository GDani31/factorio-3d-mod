// live day/night sync WITHOUT hooking: detouring DayTime::getDarkness (a
// tiny hot leaf called from the sound/render loops) crashed the game, so
// instead the game's own getter is CALLED directly on the surface's DayTime.
//
// DayTime layout (from the 2.0.77 getDarkness/setPosition/luaReadDarkness
// disassembly):
//   +0x00 dusk  +0x08 dawn  +0x10 evening  +0x18 morning   (all doubles 0..1,
//   dusk <= evening <= morning <= dawn)
//   +0x20 ticksPerDay (u64, vanilla 25200)
//   +0x28 position (double 0..1)  +0x32 automated flag
//   +0x38 max darkness (double, vanilla 0.85 — luaReadDarkness passes it as cap)
// Surface holds the DayTime* at +0x830 (luaReadDarkness: mov rcx,[rax+0x830]);
// the known offset is tried first, then a scan for a pointer matching the
// invariants above covers future layout shifts.
// getDarkness(double cap) returns min(cap, curve(position)) as float.

use crate::hooks::mem;
use crate::symbols::SymbolMap;
use crate::util::AtomicF32;
use anyhow::Result;
use std::sync::atomic::{AtomicUsize, Ordering};

// float DayTime::getDarkness(double cap) const — called, never hooked
type FnGetDarkness = unsafe extern "C" fn(usize, f64) -> f32;

static GET_DARKNESS: AtomicUsize = AtomicUsize::new(0);
// cached (surface, daytime) pair; re-scanned when the surface changes or the
// cached pointer stops validating (save reload frees the old Surface)
static CACHED_SURFACE: AtomicUsize = AtomicUsize::new(0);
static CACHED_DAYTIME: AtomicUsize = AtomicUsize::new(0);
// latest darkness 0..1 (-1 = never sampled, e.g. main menu)
static DARKNESS: AtomicF32 = AtomicF32::new(-1.0);
static LOGGED: AtomicUsize = AtomicUsize::new(0);

// Surface::dayTime member offset in 2.0.77
const SURFACE_DAYTIME: usize = 0x830;

pub fn init(symbols: &SymbolMap, base: usize) -> Result<()> {
    GET_DARKNESS.store(
        super::resolve(symbols, base, &crate::offsets::DAYTIME_GET_DARKNESS),
        Ordering::Relaxed,
    );
    Ok(())
}

// a plausible DayTime at `p`? checks every field the getter dereferences
fn validate_daytime(p: usize) -> bool {
    if p == 0 || !mem::readable(p as *const u8, 0x40) {
        return false;
    }
    let d = |off| mem::try_read::<f64>(p + off).filter(|v| v.is_finite() && (0.0..=1.0).contains(v));
    let (Some(dusk), Some(dawn), Some(evening), Some(morning), Some(_pos)) =
        (d(0x00), d(0x08), d(0x10), d(0x18), d(0x28))
    else {
        return false;
    };
    let Some(ticks) = mem::try_read::<u64>(p + 0x20) else { return false };
    dusk <= evening && evening <= morning && morning <= dawn && (100..1_000_000_000).contains(&ticks)
}

// the known member offset first, then a scan (survives layout shifts)
fn find_daytime(surface: usize) -> usize {
    let candidates = std::iter::once(SURFACE_DAYTIME).chain((0..0x1000).step_by(8));
    for off in candidates {
        let Some(p) = mem::try_read::<usize>(surface + off) else { continue };
        if validate_daytime(p) {
            if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
                log::info!("[daynight] DayTime found at surface+0x{off:X} -> 0x{p:X}");
            }
            return p;
        }
    }
    0
}

// called once per frame with the dominant surface; refreshes DARKNESS
pub fn sample(surface: usize) {
    let addr = GET_DARKNESS.load(Ordering::Relaxed);
    if addr == 0 || surface == 0 {
        return;
    }
    let mut daytime = CACHED_DAYTIME.load(Ordering::Relaxed);
    // revalidate every frame — a freed surface must never be dereferenced
    if CACHED_SURFACE.load(Ordering::Relaxed) != surface || !validate_daytime(daytime) {
        daytime = find_daytime(surface);
        CACHED_SURFACE.store(surface, Ordering::Relaxed);
        CACHED_DAYTIME.store(daytime, Ordering::Relaxed);
    }
    if daytime == 0 {
        return;
    }
    // same cap the game's own lua reader passes (max darkness, vanilla 0.85)
    let cap = mem::try_read::<f64>(daytime + 0x38)
        .filter(|c| c.is_finite() && (0.0..=1.0).contains(c))
        .unwrap_or(1.0);
    let get_darkness: FnGetDarkness = unsafe { std::mem::transmute(addr) };
    let d = unsafe { get_darkness(daytime, cap) };
    if d.is_finite() && (0.0..=1.0).contains(&d) {
        if DARKNESS.get() < 0.0 {
            log::info!("[daynight] live darkness sampling active (darkness={d:.3}, cap={cap:.2})");
        }
        DARKNESS.set(d);
    }
}

// darkness the game last computed (None until a surface was sampled)
pub fn game_darkness() -> Option<f32> {
    let d = DARKNESS.get();
    (d >= 0.0).then_some(d)
}

// the shared 0..1 night factor: the NIGHT knob when set, otherwise the live
// game darkness scaled by NIGHT_GAIN (used by model shading AND the sky)
pub fn night_factor() -> f32 {
    let knob = crate::tuning::NIGHT.get();
    if knob >= 0.0 {
        knob.min(1.0)
    } else {
        (game_darkness().unwrap_or(0.0) * crate::tuning::NIGHT_GAIN.get()).clamp(0.0, 1.0)
    }
}

// all detours into the game's code.
//
// - frame    – per-frame hooks (present, world render, render params, boost)
// - input    – cursor, walking direction, cursor->tile picking
// - mem      – safe memory probing + prototype string scanning
// - getters  – calls into the game's own getter functions
// - machines – entity draw hooks + install orchestration
// - items    – items on belts / in inserter hands / on the ground
// - wires    – wire span capture for the 3d catenaries
// - daynight – caches the game's live darkness (models follow the day cycle)

pub mod daynight;
pub mod frame;
pub mod getters;
pub mod input;
pub mod items;
pub mod machines;
pub mod mem;
pub mod player;
pub mod wires;

use crate::offsets::GameFn;
use crate::symbols::SymbolMap;
use anyhow::Result;

// factorio Vector: two doubles
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Vec2f {
    pub x: f64,
    pub y: f64,
}

// factorio MapPosition: two i32s in 1/256-tile fixed point
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MapPos {
    pub x: i32,
    pub y: i32,
}

impl MapPos {
    // read a MapPosition behind a raw pointer, in tiles
    pub unsafe fn tiles_at(addr: usize) -> (f32, f32) {
        let p = unsafe { std::ptr::read_unaligned(addr as *const MapPos) };
        (p.x as f32 / crate::offsets::RECT_FP, p.y as f32 / crate::offsets::RECT_FP)
    }

    // same, for a MapPosition packed into a u64 register (x low, y high)
    pub fn tiles_packed(pos: u64) -> (f32, f32) {
        let x = (pos & 0xFFFF_FFFF) as u32 as i32 as f32 / crate::offsets::RECT_FP;
        let y = (pos >> 32) as u32 as i32 as f32 / crate::offsets::RECT_FP;
        (x, y)
    }
}

// run mod code inside a hooked extern "C" function without letting a panic
// unwind across the ffi boundary (that aborts the whole game). the panic
// hook in lib.rs already logged message + backtrace; this logs which hook
// and returns the fallback so the vanilla path still runs
pub(crate) fn guard<R>(name: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            use std::sync::atomic::{AtomicU64, Ordering};
            static LOGGED: AtomicU64 = AtomicU64::new(0);
            if LOGGED.fetch_add(1, Ordering::Relaxed) < 20 {
                log::error!("[guard] panic caught in {name} — vanilla fallback");
            }
            fallback
        }
    }
}

// find a game function's address: by pdb symbol name first, fallback rva second
pub(crate) fn resolve(symbols: &SymbolMap, base: usize, gf: &GameFn) -> usize {
    if !gf.symbol.is_empty() {
        if let Some((name, addr)) = symbols.iter().find(|(n, _)| n.contains(gf.symbol)) {
            log::info!("resolved {} -> {name} @ 0x{addr:X}", gf.symbol);
            return *addr;
        }
        log::warn!(
            "symbol '{}' not in pdb — using fallback rva 0x{:X} (re-check offsets.rs if the game was updated)",
            gf.symbol,
            gf.rva
        );
    }
    base + gf.rva
}

// initialize a static detour on a game function (phase 1: allocates the
// trampoline, patches nothing yet — enable() does the observable write)
macro_rules! hook {
    ($symbols:expr, $base:expr, $detour:ident, $target:ident, $hooked:expr) => {
        $detour.initialize(
            std::mem::transmute(crate::hooks::resolve($symbols, $base, &crate::offsets::$target)),
            $hooked,
        )?
    };
}
pub(crate) use hook;

// install every hook
pub fn install(symbols: &SymbolMap) -> Result<()> {
    frame::create_renderer()?;
    let base = crate::symbols::base_address()?;
    log::info!("module base: 0x{base:X}");

    frame::install_early(symbols, base)?;
    input::install(symbols, base)?;
    frame::install_params(symbols, base)?;
    // entity draws are hot on the game's prepare/update threads and enable
    // their detours with all other threads suspended
    machines::install(symbols, base)?;
    Ok(())
}

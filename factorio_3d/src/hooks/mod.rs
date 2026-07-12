// all detours into the game's code.
//
// - frame    – per-frame hooks (present, world render, render params, zoom boost)
// - input    – cursor, walking direction, cursor->tile picking
// - entities – entity draw methods (player, vehicles, units, buildings)
// - sprites  – sprite queue hooks, rect recording, layer capture windows
// - rotation – camera-facing sprite rotation (frame-selection hooks)

pub mod entities;
pub mod frame;
pub mod input;
pub mod rotation;
pub mod sprites;

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

thread_local! {
    // RenderLayer of the DrawQueue call currently on this thread's stack,
    // so the placement hook (which never sees the layer) can filter.
    // 255 = not inside a tracked draw
    pub static CURRENT_DRAW_LAYER: std::cell::Cell<u8> = const { std::cell::Cell::new(255) };
    // set while Character::draw runs (the player's sprites carry no layer)
    pub static IN_CHARACTER_DRAW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // depth counter while a vehicle/unit draw runs (subclasses call their
    // base's draw, and both are hooked — hence a counter, not a bool)
    pub static ENTITY_DRAW_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    // true while EntityRenderer::prepare runs — the draw-phase gate for the
    // rotation hooks (the same functions are also called from simulation
    // code, where rotating would corrupt game state)
    pub static IN_RENDER_PREPARE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // sprites recorded inside this are laid flat (thrusters)
    pub static IN_FLAT_DRAW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // sprites recorded inside this are flying robots (lifted + shifted south)
    pub static IN_FLY_DRAW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // flat sprites that also sit on a low raised platform (solar panel, silo)
    pub static IN_FLAT_ELEVATED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // agricultural tower crane arm sprites (lifted up + shifted south)
    pub static IN_CRANE_DRAW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // depth counter while an enemy/unit draw runs (screen-bottom anchored)
    pub static IN_UNIT_DRAW: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    // depth counter for static direction-based entity draws (splitter,
    // underground belt, inserter, pipe). separate from ENTITY_DRAW_DEPTH:
    // that one routes sprites to the mobile-entity overlay
    pub static STATIC_DRAW_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    // serial of the entity currently drawing — all parts recorded inside one
    // draw call share it, which groups an entity's parts exactly
    pub static ENTITY_DRAW_SERIAL: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

// serial source for ENTITY_DRAW_SERIAL (0 means "none")
pub static ENTITY_SERIAL_NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

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

// install every hook. order matters only for the marked groups
pub fn install(symbols: &SymbolMap) -> Result<()> {
    frame::create_renderer()?;
    let base = crate::symbols::base_address()?;
    log::info!("module base: 0x{base:X}");

    frame::install_early(symbols, base)?;
    input::install(symbols, base)?;
    entities::install_actors(symbols, base)?;
    frame::install_params(symbols, base)?;
    sprites::install(symbols, base)?;
    // these patch functions that are HOT on the game's update thread and must
    // enable their detours with all other threads suspended
    rotation::install(symbols, base)?;
    entities::install_directional(symbols, base)?;
    crate::sky::install(symbols, base)?;
    Ok(())
}

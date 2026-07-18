// the player character in 3d.
//
// - Character::drawInternal is suppressed like the other entity draws;
//   position + direction + player color are recorded and the per-state glbs
//   under ENTITIES/PLAYER take over (idle / running / shoot-run / shoot-stand /
//   mining, picked in entities::update_player). drawInternal (not ::draw)
//   because on a multiplayer client the local player's visible character is
//   the LatencyState copy, drawn via drawInternal(queue, latency=true)
//   directly — Character::draw early-outs for the duplicated game-state
//   character and never runs for the copy.
// - ManualMiner::performMining runs every tick while the local player mines —
//   its call frame is the mining heartbeat (global: the manual miner belongs
//   to the local controller, fine in single player).
// - ShooterLogic::shoot<Character> fires per shot WITH the character pointer —
//   the per-character shooting heartbeat.
//
// character entries expire in entities::tick when they stop re-recording
// (player entered a vehicle / got hidden), unlike buildings which persist.

use crate::hooks::{getters, hook};
use crate::offsets;
use crate::symbols::SymbolMap;
use anyhow::Result;
use retour::static_detour;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static_detour! {
    static CharDrawHook:
        unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, u8);
    static MinerHook: unsafe extern "C" fn(*mut core::ffi::c_void) -> u8;
    static ShootHook: unsafe extern "C" fn(*mut core::ffi::c_void) -> u8;
}

// Character::getDirection / getPlayerColor, called directly (never hooked)
static CHAR_DIR: AtomicUsize = AtomicUsize::new(0);
static CHAR_COLOR: AtomicUsize = AtomicUsize::new(0);

// frame of the last performMining call (0 = never)
static MINING_FRAME: AtomicU64 = AtomicU64::new(0);
static COLOR_LOGGED: AtomicU64 = AtomicU64::new(0);

pub fn mining_frame() -> u64 {
    MINING_FRAME.load(Ordering::Relaxed)
}

// Color returns by value: 16 bytes with a ctor -> hidden-pointer convention
// (same dance as getters.rs). validated to sane 0..1 rgba
type FnGetColor = unsafe extern "C" fn(*const core::ffi::c_void, *mut [f32; 4]) -> u64;

fn player_color(this: *mut core::ffi::c_void) -> Option<[f32; 4]> {
    let addr = CHAR_COLOR.load(Ordering::Relaxed);
    if addr == 0 || this.is_null() {
        return None;
    }
    let get_color: FnGetColor = unsafe { std::mem::transmute(addr) };
    let mut buf = [f32::NAN; 4];
    let rax = unsafe { get_color(this, &mut buf) };
    if !buf.iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c)) {
        // pod fallback: two packed floats per register would land in rax —
        // log once so a wrong convention is diagnosable, then give up
        if COLOR_LOGGED.fetch_add(1, Ordering::Relaxed) == 0 {
            log::warn!("[player] getPlayerColor buf {buf:?} rax 0x{rax:016X} — no tint");
        }
        return None;
    }
    Some(buf)
}

fn hooked_char_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void, latency: u8) {
    // a panic here would abort the game — fall back to the vanilla draw
    let suppress = super::guard("char_draw", false, || char_draw_inner(this, queue, latency != 0));
    if !suppress {
        unsafe { CharDrawHook.call(this, queue, latency) };
    }
}

// true = the 3d model takes over, the vanilla sprite draw is skipped
fn char_draw_inner(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void, latency: bool) -> bool {
    let model = if this.is_null() { None } else { getters::entity_model(this) };
    let Some(model) = model else { return false };
    if crate::hooks::mem::dq_is_ghost(queue as usize) || super::machines::in_gui_preview() {
        return false;
    }
    let Some((x, y)) = getters::entity_pos_field(this) else { return false };
    let dir = getters::entity_dir(this, CHAR_DIR.load(Ordering::Relaxed));
    // the latency copy lives on the private LatencySurface — its pointer never
    // matches the dominant surface, so report "unknown" (0) or the surface
    // cull would hide the model of the character we just suppressed
    let surface = if latency { 0 } else { getters::entity_surface(this) };
    // fingerprint = frame counter: a character always animates (idle sway),
    // and the per-frame re-record doubles as the liveness signal for expiry
    let suppress = crate::entities::record(crate::entities::Record {
        dir,
        surface,
        fingerprint: crate::hooks::frame::frame_count(),
        ..crate::entities::Record::at(this as usize, &model, x, y)
    });
    if let Some(c) = player_color(this) {
        crate::entities::set_player_color(this as usize, c);
    }
    suppress
}

fn hooked_perform_mining(this: *mut core::ffi::c_void) -> u8 {
    MINING_FRAME.store(crate::hooks::frame::frame_count(), Ordering::Relaxed);
    unsafe { MinerHook.call(this) }
}

fn hooked_shoot(character: *mut core::ffi::c_void) -> u8 {
    let fired = unsafe { ShootHook.call(character) };
    if fired != 0 && !character.is_null() {
        super::guard("shoot", (), || crate::entities::set_player_shot(character as usize));
    }
    fired
}

// phase 1: initialize detours + resolve the getters (nothing observable yet)
pub fn init(symbols: &SymbolMap, base: usize) -> Result<()> {
    let resolve = |gf| super::resolve(symbols, base, gf);
    CHAR_DIR.store(resolve(&offsets::CHARACTER_GET_DIRECTION), Ordering::Relaxed);
    CHAR_COLOR.store(resolve(&offsets::CHARACTER_GET_PLAYER_COLOR), Ordering::Relaxed);
    unsafe {
        hook!(symbols, base, CharDrawHook, CHARACTER_DRAW_INTERNAL, hooked_char_draw);
        hook!(symbols, base, MinerHook, MANUAL_MINER_PERFORM, hooked_perform_mining);
        hook!(symbols, base, ShootHook, SHOOTER_SHOOT_CHARACTER, hooked_shoot);
    }
    Ok(())
}

// phase 2: called inside with_other_threads_suspended (no alloc/log here)
pub fn enable() -> Result<()> {
    unsafe {
        CharDrawHook.enable()?;
        MinerHook.enable()?;
        ShootHook.enable()?;
    }
    Ok(())
}

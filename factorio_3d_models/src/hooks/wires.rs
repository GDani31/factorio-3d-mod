// wire hooks: capture every wire span (copper + circuit) and swallow the
// vanilla sprite — the renderer draws 3d catenaries from the captured spans.

use crate::hooks::mem::{self, dq_is_ghost, readable};
use crate::hooks::{MapPos, hook};
use crate::offsets;
use crate::symbols::SymbolMap;
use crate::util::memo;
use anyhow::Result;
use retour::static_detour;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

// WireRendering::drawWires(DrawQueue&, WireConnector const&, WireStyle)
type FnDrawWires = unsafe extern "C" fn(usize, usize, usize);

static_detour! {
    // WireRendering::draw(DrawQueue&, MapPosition const&, MapPosition const&,
    //   double height, NamedBool<WireShadowTag>, Sprite const&, Color const&, RenderLayer)
    static WireSegHook: unsafe extern "C" fn(usize, usize, usize, f64, usize, usize, usize, usize);
}

static DRAW_WIRES_ADDR: AtomicUsize = AtomicUsize::new(0);
static WIRES_LOGGED: AtomicUsize = AtomicUsize::new(0);

// --- wire color from the sprite -------------------------------------------------------

static WIRE_COLOR_CACHE: LazyLock<Mutex<HashMap<usize, [f32; 4]>>> =
    LazyLock::new(Default::default);

// collect strings from the sprite itself AND one pointer-hop away (the sprite's
// name/file lives behind a leading pointer, not inline — a direct scan is empty)
fn wire_sprite_strings(sprite: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if sprite == 0 || !readable(sprite as *const u8, 0x80) {
        return out;
    }
    for (_, s) in mem::scan_proto_strings(sprite as *const u8) {
        out.push(s);
    }
    for off in (0..0x80usize).step_by(8) {
        let sub: usize = mem::read(sprite + off);
        if sub == 0 || sub == sprite || !readable(sub as *const u8, 0x40) {
            continue;
        }
        for (_, s) in mem::scan_proto_strings(sub as *const u8) {
            out.push(s);
        }
    }
    out
}

// the FIRST specific "<color>-wire" filename wins — a wire sprite lists its own
// name before the shared atlas index (which contains copper AND green AND ui),
// so scanning for "green" anywhere wrongly greened copper wires
const COPPER: [f32; 4] = [0.72, 0.45, 0.28, 1.0];

fn wire_color_from(strings: &[String]) -> Option<[f32; 4]> {
    strings.iter().find_map(|s| {
        let s = s.to_lowercase();
        if s.contains("copper-wire") {
            Some(COPPER)
        } else if s.contains("green-wire") {
            Some([0.25, 0.75, 0.28, 1.0])
        } else if s.contains("red-wire") {
            Some([0.85, 0.25, 0.22, 1.0])
        } else {
            None
        }
    })
}

fn wire_color(sprite: usize) -> [f32; 4] {
    memo(&WIRE_COLOR_CACHE, sprite, || {
        // the sprite's OWN std::strings (no pointer hops) name it authoritatively;
        // hopping leaks the shared atlas index that lists every wire + ui icon
        let direct = mem::scan_proto_strings(sprite as *const u8)
            .into_iter()
            .map(|(_, s)| s)
            .collect::<Vec<_>>();
        let hopped = wire_sprite_strings(sprite);
        let color = wire_color_from(&direct)
            .or_else(|| wire_color_from(&hopped))
            .unwrap_or(COPPER); // copper is the common case
        if WIRES_LOGGED.fetch_add(1, Ordering::Relaxed) < 16 {
            log::info!(
                "[wires] sprite @0x{sprite:X}: direct {direct:?} -> {color:?} (hopped-first {:?})",
                wire_color_from(&hopped)
            );
        }
        color
    })
}

// --- the span hook ---------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn hooked_wire_seg(
    dq: usize, from: usize, to: usize, height: f64, shadow: usize,
    sprite: usize, color: usize, layer: usize,
) {
    if dq_is_ghost(dq) || from == 0 || to == 0 {
        unsafe { WireSegHook.call(dq, from, to, height, shadow, sprite, color, layer) };
        return;
    }
    // the shadow pass is dropped entirely; the main span is recorded and
    // swallowed — the 3d catenary pass replaces it
    if shadow & 0xFF != 0 {
        return;
    }
    // the tint argument distinguishes circuit wires; near-white = copper
    let tint = if color != 0 && readable(color as *const u8, 16) {
        let c = mem::read::<[f32; 4]>(color);
        (c.iter().all(|v| (0.0..=1.001).contains(v)) && (c[0] < 0.93 || c[1] < 0.93 || c[2] < 0.93))
            .then_some(c)
    } else {
        None
    };
    let (x1, y1) = unsafe { MapPos::tiles_at(from) };
    let (x2, y2) = unsafe { MapPos::tiles_at(to) };
    let w = crate::entities::WireDraw {
        x1,
        y1,
        x2,
        y2,
        h: height as f32,
        color: tint.unwrap_or_else(|| wire_color(sprite)),
        seen_frame: 0, // stamped in record_wire
    };
    if WIRES_LOGGED.fetch_add(1, Ordering::Relaxed) < 12 {
        let c = if color != 0 && readable(color as *const u8, 16) {
            mem::read::<[f32; 4]>(color)
        } else {
            [0.0; 4]
        };
        log::info!(
            "[wires] span ({:.1},{:.1})->({:.1},{:.1}) height {height:.2} tint {c:?} sprite 0x{sprite:X} layer {}",
            w.x1, w.y1, w.x2, w.y2, layer & 0xFF
        );
    }
    crate::entities::record_wire(w);
}

// a suppressed pole's ::draw would have submitted its wires — call drawWires
// directly so the wire hook still captures them. stale spans around the pole
// drop first, live ones re-record
pub(crate) fn submit_pole_wires(queue: usize, entity: usize, wire_off: usize, x: f32, y: f32) {
    let addr = DRAW_WIRES_ADDR.load(Ordering::Relaxed);
    if addr == 0 {
        return;
    }
    crate::entities::clear_wires_near(x, y);
    let draw_wires: FnDrawWires = unsafe { std::mem::transmute(addr) };
    unsafe { draw_wires(queue, entity + wire_off, 0) };
}

// --- install ---------------------------------------------------------------------------

pub(crate) fn init(symbols: &SymbolMap, base: usize) -> Result<()> {
    unsafe {
        hook!(symbols, base, WireSegHook, WIRE_DRAW_SEGMENT, hooked_wire_seg);
    }
    DRAW_WIRES_ADDR.store(super::resolve(symbols, base, &offsets::WIRE_DRAW_WIRES), Ordering::Relaxed);
    Ok(())
}

pub(crate) fn enable() -> Result<()> {
    unsafe { WireSegHook.enable()? };
    Ok(())
}

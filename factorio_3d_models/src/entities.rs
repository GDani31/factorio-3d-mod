// registry of every replaced entity, fed by the draw hooks.
//
// entity draws only run when the game re-queues a chunk, so entries persist
// between draws: a working machine's animation advances every tick, which
// re-queues its chunk every frame — so "the fingerprint changed recently"
// is the working signal. entries are removed by the ~Entity hook, so stale
// pointers never linger.

use crate::models::{ConnKind, EntityModel, YawSource};
use crate::tuning;
use crate::util::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

// a caught panic while a registry lock was held must not poison-cascade into
// every other hook — keep the data (worst case: one stale entry)
fn plock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct Entry {
    model: EntityModel,
    x: f32, // tiles, entity center
    y: f32,
    dir: u8,          // 0..16 building direction
    orientation: f32, // 0..1 smooth orientation (turrets/vehicles), NAN = none
    turret: f32,        // 0..1 turret orientation relative to the body (tank), NAN = none
    track_phase: f32,   // signed tiles driven, left chain (wraps in the renderer)
    track_phase_b: f32, // right chain — differs while turning in place
    prev_x: f32,        // last tick's position, for the track phase delta
    prev_y: f32,
    prev_orient: f32, // last tick's orientation, for the turn delta (NAN = unset)
    // Surface* the entity lives on (0 = unknown). space-age platform views
    // render through the same hooks with OVERLAPPING coordinates — only the
    // dominant surface's entities draw
    surface: usize,
    rocket_flight: f32, // raw flight height from the game (rockets only), NAN = not a rocket
    first_x: f32,       // position when first seen (silo anchor for the rocket)
    first_y: f32,
    fingerprint: u64,
    first_seen_frame: u64,
    last_change_frame: u64,
    anim_t: f32,
    anim_rate: f32, // animation speed multiplier (inserters: real rotation_speed / base)
    // live inserter arm angle (RealOrientation, 0..1), NAN = not an inserter or
    // unread. its per-frame CHANGE paces the arm clip so it tracks the real arm
    arm_angle: f32,
    prev_arm_angle: f32, // last frame's arm angle, for the clip-advance delta
    swing_mirror: bool,  // mirror the inserter model so the arm swings the way
                         // the real arm does for this facing (set from arm side)
    show: bool, // false = unsupported connection shape, vanilla sprite stays
    active: bool,       // live working state (accumulator charge arcs), default true
    active_frame: u64,  // when `active` was last written — stale reads count as idle
    // dead reckoning for flying robots: their position may only re-record on
    // sparse chunk redraws (~1/s), so the renderer extrapolates along the
    // velocity measured between records instead of teleport-stepping
    vel_x: f32,     // tiles per frame, from the last two recorded positions
    vel_y: f32,
    pos_frame: u64,    // frame the recorded position last changed
    rec_interval: f32, // frames between the last two position records
    spin_yaw: f32,     // accumulated dish rotation (YawSource::Spin parts)
    // live shapekey weight override (gate: 1 - openingProgress), NAN = none —
    // the renderer then falls back to the glb's own weight animation
    morph: f32,

    // --- player character (is_player entries only) --------------------------
    is_player: bool,
    player_state: u8,        // index into models::PLAYER_KEYS
    player_color: [f32; 4],  // /color player color, a=0 until read
    player_lift: f32,        // smoothed belt-top lift, tiles
    last_move_frame: u64,    // last frame the raw position changed
    last_shot_frame: u64,    // last frame ShooterLogic fired for this character
    smooth_x: f32,           // ema-smoothed render position: the raw field
    smooth_y: f32,           // steps at 60UPS and judders against the camera
    speed_ema: f32,          // tiles/sec, paces the run clip
    move_accum: f32,         // seconds since the last position step
}

impl Entry {
    // fresh entry for a first-time record
    fn new(r: &Record, frame: u64) -> Self {
        Self {
            model: r.model.clone(),
            x: r.x,
            y: r.y,
            dir: r.dir,
            orientation: r.orientation,
            turret: r.turret,
            track_phase: 0.0,
            track_phase_b: 0.0,
            prev_x: r.x,
            prev_y: r.y,
            prev_orient: f32::NAN,
            surface: r.surface,
            rocket_flight: f32::NAN,
            first_x: r.x,
            first_y: r.y,
            fingerprint: r.fingerprint,
            first_seen_frame: frame,
            last_change_frame: 0,
            anim_t: 0.0,
            anim_rate: 1.0,
            arm_angle: f32::NAN,
            prev_arm_angle: f32::NAN,
            swing_mirror: false,
            show: true,
            active: true,
            active_frame: 0,
            vel_x: 0.0,
            vel_y: 0.0,
            pos_frame: frame,
            rec_interval: 0.0,
            spin_yaw: 0.0,
            morph: f32::NAN,
            is_player: crate::models::is_player_key(r.model.parts[0].key),
            player_state: 0,
            player_color: [0.0; 4],
            player_lift: 0.0,
            last_move_frame: 0,
            last_shot_frame: 0,
            smooth_x: r.x,
            smooth_y: r.y,
            speed_ema: 0.0,
            move_accum: 0.0,
        }
    }
}

// one model part instance for the render pass
#[derive(Clone, Default)]
pub struct Instance {
    pub key: &'static str,
    // the part whose xz extent is fitted to the footprint; all parts of an
    // entity share it so multi-part models keep their relative proportions
    pub scale_ref: &'static str,
    pub tiles: f32, // entity footprint (widest axis)
    pub x: f32,
    pub y: f32,
    pub lift: f32,    // extra height in tiles (items riding on belts)
    pub yaw: f32,     // radians
    pub mirror: bool, // mirror on x (the other belt curve direction)
    pub uv_scroll: [f32; 2], // texture scroll on upward faces (belt treads)
    // plane-space direction to clip AGAINST: only the half of the model on
    // that side of the entity center renders (composed pipe/wall junctions)
    pub clip_dir: [f32; 2],
    pub roll: f32, // rotation around the model's z axis (pipe elbows)
    pub anim_t: f32,
    pub turret_yaw: f32,    // node-space yaw for "turrethull" nodes (tank)
    pub track_phase: f32,   // signed tiles driven, left chain
    pub track_phase_b: f32, // right chain (differential turning)
    // rgba multiplied onto tintable prims (player color); a=0 = no tint
    pub tint: [f32; 4],
    // live working state — active-only prims (accumulator arcs) skip when false
    pub active: bool,
    // 0 = opaque (default); >0 fades the model (ghost-entity preview)
    pub transparency: f32,
    // shapekey weight override (gate leaf sinking); None = the glb's own
    // weight animation sampled at anim_t
    pub morph: Option<f32>,
}

// everything a draw hook knows about the entity it's recording. Record::at
// fills the common base; the hooks override what they actually have
pub struct Record<'a> {
    pub entity: usize,
    pub model: &'a EntityModel,
    pub x: f32,
    pub y: f32,
    pub dir: u8,
    pub orientation: f32, // NAN = the entity has no smooth orientation
    pub turret: f32,      // NAN = no relative turret
    pub surface: usize,
    pub fingerprint: u64,
}

impl<'a> Record<'a> {
    pub fn at(entity: usize, model: &'a EntityModel, x: f32, y: f32) -> Self {
        Self {
            entity,
            model,
            x,
            y,
            dir: 0,
            orientation: f32::NAN,
            turret: f32::NAN,
            surface: 0,
            fingerprint: 0,
        }
    }
}

// items drawn this frame (fed by the item hooks, drained by tick)
#[derive(Clone, Copy)]
pub struct ItemDraw {
    pub key: &'static str,
    pub x: f32,
    pub y: f32,
    // extra height + footprint override, tiles. NAN = use the belt defaults
    // (ITEM_LIFT / ITEM_SIZE). inserter-hand items ride higher and smaller so
    // they read as held in the hand rather than lying on the floor
    pub lift: f32,
    pub size: f32,
}

// non-belt items (inserter hands, machine i/o) are recorded into STAGING
// during draw; tick promotes each non-empty staging batch to LIVE and keeps
// LIVE on screen until the next batch arrives.
static ITEM_BUF: Mutex<Vec<ItemDraw>> = Mutex::new(Vec::new());
static LIVE_ITEMS: Mutex<Vec<ItemDraw>> = Mutex::new(Vec::new());
static LIVE_FRAME: AtomicU64 = AtomicU64::new(0);

// belt items live in their own map keyed by the belt entity: each drawItems
// call atomically replaces just ITS belt's items, so belts that didn't
// re-record this frame keep theirs. the old design swapped one global batch
// per frame — on busy maps a frame's records got split across the tick
// boundary by the parallel prepare threads (and clipped by the global cap),
// so whole belts' items dropped for a frame -> flicker on long/full belts
static BELT_ITEMS: LazyLock<Mutex<FxHashMap<usize, (u64, Vec<ItemDraw>)>>> =
    LazyLock::new(Default::default);

thread_local! {
    // collects the records made during one belt's drawItems (items.rs bracket);
    // None = not inside a belt draw, records go to the global staging buffer
    static BELT_COLLECT: std::cell::RefCell<Option<Vec<ItemDraw>>> =
        const { std::cell::RefCell::new(None) };
}

// safety caps so a hook gone wrong can't grow these buffers forever
const MAX_ITEMS: usize = 16384;
const MAX_BELT_ITEMS: usize = 256; // per belt entity
const MAX_BELTS: usize = 16384;
const MAX_GHOSTS: usize = 256;
const MAX_WIRES: usize = 4096;

static RECORD_ITEM_N: AtomicU64 = AtomicU64::new(0);

pub fn record_item(key: &'static str, x: f32, y: f32) {
    record_item_ex(key, x, y, f32::NAN, f32::NAN);
}

// like record_item but with an explicit height + footprint (inserter hands)
pub fn record_item_ex(key: &'static str, x: f32, y: f32, lift: f32, size: f32) {
    RECORD_ITEM_N.fetch_add(1, Ordering::Relaxed);
    let it = ItemDraw { key, x, y, lift, size };
    let in_belt = BELT_COLLECT.with(|c| {
        if let Some(v) = c.borrow_mut().as_mut() {
            if v.len() < MAX_BELT_ITEMS {
                v.push(it);
            }
            true
        } else {
            false
        }
    });
    if in_belt {
        return;
    }
    let mut buf = plock(&ITEM_BUF);
    if buf.len() < MAX_ITEMS {
        buf.push(it);
    }
}

// bracket one belt's drawItems: everything recorded in between replaces that
// belt's item set atomically (an empty draw clears it)
pub fn begin_belt_items() {
    BELT_COLLECT.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

pub fn end_belt_items(belt: usize) {
    let Some(items) = BELT_COLLECT.with(|c| c.borrow_mut().take()) else { return };
    let frame = crate::hooks::frame::frame_count();
    let mut map = plock(&BELT_ITEMS);
    if items.is_empty() {
        map.remove(&belt);
        return;
    }
    if let Some((f, v)) = map.get_mut(&belt) {
        // a second drawItems call for the same belt in the same frame is
        // another lane/connector — merge, don't wipe the first lane
        if *f == frame {
            v.extend(items);
        } else {
            *f = frame;
            *v = items;
        }
    } else if map.len() < MAX_BELTS {
        map.insert(belt, (frame, items));
    }
}

// --- ghost-entity previews ----------------------------------------------------------
// the game draws blueprint ghosts, dead-building ghosts and hover previews in
// DrawQueue ghost mode. those go through the entity ::draw hooks, so we record
// them here and render a transparent 3d model on top of the vanilla sprite

struct GhostDraw {
    model: EntityModel,
    x: f32,
    y: f32,
    dir: u8,
    frame: u64,
}

static GHOSTS: Mutex<Vec<GhostDraw>> = Mutex::new(Vec::new());

pub fn record_ghost(model: &EntityModel, x: f32, y: f32, dir: u8) {
    let frame = crate::hooks::frame::frame_count();
    let mut g = plock(&GHOSTS);
    if g.first().is_some_and(|d| d.frame != frame) {
        g.clear(); // new frame: drop the previous frame's ghosts
    }
    let key = model.parts[0].key;
    // last record wins: the cursor preview records twice per frame — first
    // from the drawEntityToBeBuilt hook, then from the temp entity's own
    // draw, which carries the same direction the rotated vanilla sprite uses
    if let Some(d) = g.iter_mut().find(|d| d.model.parts[0].key == key && d.x == x && d.y == y)
    {
        d.dir = dir;
        d.frame = frame;
    } else if g.len() < MAX_GHOSTS {
        g.push(GhostDraw { model: model.clone(), x, y, dir, frame });
    }
}

// transparent instances for the ghosts recorded in the last couple of frames
fn ghost_instances(frame: u64, out: &mut Vec<Instance>) {
    let alpha = tuning::GHOST_ALPHA.get().clamp(0.0, 1.0);
    if alpha <= 0.01 {
        return;
    }
    for gd in plock(&GHOSTS).iter().filter(|d| frame.saturating_sub(d.frame) <= 2) {
        let poff = tuning::model_offset_extra(gd.model.parts[0].key);
        for part in &gd.model.parts {
            out.push(Instance {
                key: part.key,
                scale_ref: gd.model.parts[0].key,
                tiles: gd.model.tiles,
                x: gd.x + poff[0],
                y: gd.y + poff[1],
                lift: poff[2],
                yaw: dir_to_yaw(gd.dir)
                    + key_extra_yaw(part.key, false)
                    + tuning::model_yaw_extra(part.key),
                transparency: 1.0 - alpha,
                ..Instance::default()
            });
        }
    }
}

// --- wires ------------------------------------------------------------------------------
// captured wire spans (copper + circuit), replaced by 3d catenaries.
// wires only re-submit when their chunk re-queues, so entries PERSIST keyed
// by endpoints; they're cleared around a pole right before it re-submits
// (stale wires drop, surviving ones re-record) and when an entity dies

#[derive(Clone)]
pub struct WireDraw {
    pub x1: f32, // tiles
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub h: f32, // the game's height value for the span (render height)
    pub color: [f32; 4],
    pub seen_frame: u64, // last frame this exact span was recorded (for expiry)
}

type WireKey = (i32, i32, i32, i32, u32);
static WIRES: LazyLock<Mutex<FxHashMap<WireKey, WireDraw>>> = LazyLock::new(Default::default);
static WIRE_RECORDS: AtomicU64 = AtomicU64::new(0);

fn wire_key(w: &WireDraw) -> WireKey {
    let q = |v: f32| (v * 16.0).round() as i32;
    (q(w.x1), q(w.y1), q(w.x2), q(w.y2), w.color[0].to_bits() ^ w.color[1].to_bits().rotate_left(8))
}

pub fn record_wire(mut w: WireDraw) {
    w.seen_frame = crate::hooks::frame::frame_count();
    WIRE_RECORDS.fetch_add(1, Ordering::Relaxed);
    let mut map = plock(&WIRES);
    if map.len() < MAX_WIRES || map.contains_key(&wire_key(&w)) {
        map.insert(wire_key(&w), w);
    }
}

// drop every wire with an endpoint near (x, y) — called before a pole
// re-submits its wires and when an entity is destroyed
pub fn clear_wires_near(x: f32, y: f32) {
    let near = |wx: f32, wy: f32| {
        let (dx, dy) = (wx - x, wy - y);
        dx * dx + dy * dy < 1.0
    };
    plock(&WIRES).retain(|_, w| !near(w.x1, w.y1) && !near(w.x2, w.y2));
}

// snapshot for this frame's render. real wires (both ends anchored on a
// registered entity — poles, combinators...) PERSIST between chunk redraws,
// since they only re-record on hover/redraw. a span with an endpoint hanging
// in open space is a manual-wire-drag preview: those expire after
// WIRE_EXPIRE_FRAMES, clearing the fan of orphans left as the cursor moves
pub fn take_wires() -> Vec<WireDraw> {
    let frame = crate::hooks::frame::frame_count();
    let window = tuning::WIRE_EXPIRE_FRAMES.get().max(1.0) as u64;
    let mut map = plock(&WIRES);
    // the anchor check scans EVERY registered entity — run it every few
    // frames, not per frame (orphaned drag previews just live ~8 frames longer)
    if frame % 8 == 0 {
        // cells occupied by registered entities (wires dock at these)
        let cells: FxHashSet<(i32, i32)> =
            plock(&ENTITIES).values().map(|e| cell_of(e.x, e.y)).collect();
        let near_entity = |x: f32, y: f32| {
            let (cx, cy) = cell_of(x, y);
            (-1..=1).any(|dx| (-1..=1).any(|dy| cells.contains(&(cx + dx, cy + dy))))
        };
        map.retain(|_, w| {
            let anchored = near_entity(w.x1, w.y1) && near_entity(w.x2, w.y2);
            anchored || frame.saturating_sub(w.seen_frame) < window
        });
    }
    if frame % 300 == 0 {
        log::info!(
            "[wires] {} live spans, {} records/interval, window {} frames",
            map.len(),
            WIRE_RECORDS.swap(0, Ordering::Relaxed),
            window
        );
    }
    map.values().cloned().collect()
}

// --- the registry -------------------------------------------------------------------------

static ENTITIES: LazyLock<Mutex<FxHashMap<usize, Entry>>> = LazyLock::new(Default::default);
static RECORD_COUNT: AtomicU64 = AtomicU64::new(0);
static NEW_LOGGED: AtomicU64 = AtomicU64::new(0);

// connectable-entity grid: tile cell -> (kind, direction). belts/pipes/walls
// look up their neighbors here to pick a model variant + rotation
type Grid = FxHashMap<(i32, i32), (ConnKind, u8)>;
static GRID: LazyLock<Mutex<Grid>> = LazyLock::new(Default::default);

// the model an entity was registered with (ground-item adoption check)
pub fn model_of(entity: usize) -> Option<EntityModel> {
    plock(&ENTITIES).get(&entity).map(|e| e.model.clone())
}

// mark a registered entity as the silo rocket and give it the game's raw
// flight height; the animated display lift is computed in tick
pub fn set_rocket(entity: usize, flight: f32) {
    if let Some(e) = plock(&ENTITIES).get_mut(&entity) {
        e.rocket_flight = flight;
    }
}

// per-entity animation speed multiplier (inserters: real rotation_speed / base)
pub fn set_anim_rate(entity: usize, rate: f32) {
    if let Some(e) = plock(&ENTITIES).get_mut(&entity) {
        e.anim_rate = rate;
    }
}

// the inserter's live arm angle (RealOrientation) — drives the arm animation
pub fn set_arm_angle(entity: usize, angle: f32) {
    if let Some(e) = plock(&ENTITIES).get_mut(&entity) {
        e.arm_angle = angle;
    }
}

// the character's /color player color, read during its draw
pub fn set_player_color(entity: usize, color: [f32; 4]) {
    if let Some(e) = plock(&ENTITIES).get_mut(&entity) {
        e.player_color = color;
    }
}

// live shapekey weight (gate: 1 - openingProgress, 0 = closed/extended)
pub fn set_morph(entity: usize, w: f32) {
    if let Some(e) = plock(&ENTITIES).get_mut(&entity) {
        e.morph = w;
    }
}

// live working state (accumulator: nonzero activity rate = charging arcs on)
pub fn set_active(entity: usize, active: bool) {
    if let Some(e) = plock(&ENTITIES).get_mut(&entity) {
        e.active = active;
        e.active_frame = crate::hooks::frame::frame_count();
    }
}

// this character fired a shot this frame (ShooterLogic hook)
pub fn set_player_shot(entity: usize) {
    if let Some(e) = plock(&ENTITIES).get_mut(&entity) {
        e.last_shot_frame = crate::hooks::frame::frame_count();
    }
}

// tile cells occupied by electric-pole entities (NOT substations). the wire
// renderer raises wire ends that land on these so the wire meets the taller
// 3d pole top; substations already line up so they're excluded.
// a full registry scan — cached and refreshed every ~half second, poles
// don't move (a scan per frame scaled with the whole explored base)
pub fn electric_pole_cells() -> std::sync::Arc<HashSet<(i32, i32)>> {
    static CACHE: LazyLock<Mutex<(u64, std::sync::Arc<HashSet<(i32, i32)>>)>> =
        LazyLock::new(|| Mutex::new((u64::MAX, Default::default())));
    let frame = crate::hooks::frame::frame_count();
    let mut cache = plock(&CACHE);
    if cache.0 == u64::MAX || frame.saturating_sub(cache.0) >= 30 {
        let cells = plock(&ENTITIES)
            .values()
            .filter(|e| e.model.parts[0].key.contains("electric-pole"))
            .map(|e| cell_of(e.x, e.y))
            .collect();
        *cache = (frame, std::sync::Arc::new(cells));
    }
    cache.1.clone()
}

// belt-record throttle: belts re-record every frame while carrying items,
// but they're static — after registration a full record() (ENTITIES + GRID
// locks, contending with tick's long hold) is only needed every ~half second
// to pick up rotations. own tiny lock so prepare threads never touch the
// big registry lock on the fast path
static BELT_SEEN: LazyLock<Mutex<crate::util::FxHashMap<usize, u64>>> =
    LazyLock::new(Default::default);

pub fn belt_record_due(belt: usize) -> bool {
    let frame = crate::hooks::frame::frame_count();
    let mut seen = plock(&BELT_SEEN);
    match seen.entry(belt) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            if frame.saturating_sub(*e.get()) < 30 {
                false
            } else {
                e.insert(frame);
                true
            }
        }
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(frame);
            true
        }
    }
}

// register a ground-item entity with its item model — from then on it
// behaves like any replaced entity (persists, cleaned up by ~Entity)
pub fn adopt_item(entity: usize, key: &'static str, x: f32, y: f32, surface: usize) {
    let model = EntityModel {
        parts: vec![crate::models::Part { key, yaw: YawSource::Fixed }],
        tiles: tuning::ITEM_SIZE.get(),
        kind: ConnKind::None,
    };
    record(Record { surface, ..Record::at(entity, &model, x, y) });
}

// called from the draw hooks (prepare worker threads). returns true when the
// model is ready to replace the vanilla sprite. non-animating entities
// (belts, walls, poles...) draw ONCE per chunk re-queue, so suppression
// happens on the first draw — the build-cursor preview is filtered by the
// ghost-mode check in the hooks
pub fn record(r: Record) -> bool {
    let frame = crate::hooks::frame::frame_count();
    let mut map = plock(&ENTITIES);
    if !map.contains_key(&r.entity) && NEW_LOGGED.fetch_add(1, Ordering::Relaxed) < 80 {
        log::info!(
            "[entities] new: @0x{:X} {} at ({:.1},{:.1}) dir {} orient {:.2}",
            r.entity, r.model.parts[0].key, r.x, r.y, r.dir, r.orientation
        );
    }
    let e = map.entry(r.entity).or_insert_with(|| Entry::new(&r, frame));
    let established = frame > e.first_seen_frame + 1;
    if e.fingerprint != r.fingerprint {
        e.fingerprint = r.fingerprint;
        e.last_change_frame = frame;
    }
    // velocity between recorded positions (dead reckoning for robots). the
    // teleport test is on SPEED, not distance — a fast robot recording once a
    // second legitimately jumps many tiles per record (0.5 tiles/frame =
    // 30 tiles/s, far above any bot; map transfers are way beyond it)
    if r.x != e.x || r.y != e.y {
        let (dx, dy) = (r.x - e.x, r.y - e.y);
        let df = frame.saturating_sub(e.pos_frame).max(1) as f32;
        if (dx.abs() + dy.abs()) / df > 0.5 {
            e.vel_x = 0.0;
            e.vel_y = 0.0;
        } else {
            e.vel_x = dx / df;
            e.vel_y = dy / df;
            e.rec_interval = df.min(120.0);
        }
        e.pos_frame = frame;
    }
    e.x = r.x;
    e.y = r.y;
    e.dir = r.dir;
    e.orientation = r.orientation;
    e.turret = r.turret;
    if r.surface != 0 {
        e.surface = r.surface;
    }

    // connectable entities register in the neighbor grid. always show:
    // falling back to the vanilla sprite creates HOLES, because the sprite is
    // already suppressed in the cached chunk — unsupported junction shapes
    // render the straight model instead
    if r.model.kind != ConnKind::None {
        let mut grid = plock(&GRID);
        if r.model.kind == ConnKind::Splitter {
            for c in splitter_cells(r.x, r.y, r.dir) {
                grid.insert(c, (r.model.kind, r.dir));
            }
        } else {
            grid.insert(cell_of(r.x, r.y), (r.model.kind, r.dir));
        }
        e.show = true;
    }
    let show = e.show;

    let n = RECORD_COUNT.fetch_add(1, Ordering::Relaxed);
    if n % 900 == 0 {
        log::info!(
            "[entities] sample: pos ({:.1},{:.1}) dir {} orient {:.2} fp 0x{:016X} established {established} (entities: {})",
            r.x, r.y, r.dir, r.orientation, r.fingerprint,
            map.len()
        );
    }
    // the 3d model renders from the FIRST record (tick doesn't gate on age),
    // but the vanilla sprite is only suppressed once the entity has been
    // around for 2+ frames — so the cursor placement preview stays visible
    // (as vanilla sprite + 3d model) and non-animating entities like belts
    // show their model immediately, layered over the cached sprite until
    // the chunk re-renders
    established && show && crate::models::all_ready(r.model)
}

// called from the ~Entity hook (every entity type funnels through it)
// and from the die hook (destroyed buildings show their remnants only)
pub fn remove(entity: usize) {
    plock(&BELT_ITEMS).remove(&entity);
    plock(&BELT_SEEN).remove(&entity);
    let Some(e) = plock(&ENTITIES).remove(&entity) else { return };
    if e.rocket_flight.is_finite() {
        let age = crate::hooks::frame::frame_count().saturating_sub(e.first_seen_frame);
        log::info!("[rocket] removed after {age} frames (~{:.1}s)", age as f32 / 60.0);
    }
    if e.model.kind != ConnKind::None {
        let mut grid = plock(&GRID);
        if e.model.kind == ConnKind::Splitter {
            for c in splitter_cells(e.x, e.y, e.dir) {
                grid.remove(&c);
            }
        } else {
            grid.remove(&cell_of(e.x, e.y));
        }
    }
    clear_wires_near(e.x, e.y);
}

// --- geometry helpers -----------------------------------------------------------------------

fn cell_of(x: f32, y: f32) -> (i32, i32) {
    (x.floor() as i32, y.floor() as i32)
}

// same connectivity group? (pipe-to-ground counts as a pipe neighbor,
// underground belts and splitters count as belt neighbors, gates as walls)
fn same_group(a: ConnKind, b: ConnKind) -> bool {
    use ConnKind::*;
    matches!(
        (a, b),
        (Belt | Under | Splitter, Belt | Under | Splitter)
            | (Pipe | PipeEnd, Pipe | PipeEnd)
            | (Heat, Heat)
            | (Wall | Gate, Wall | Gate)
    )
}

// a splitter is 2 tiles wide: its two grid cells sit either side of center
fn splitter_cells(x: f32, y: f32, dir: u8) -> [(i32, i32); 2] {
    let (ox, oy) = if card(dir) % 2 == 0 { (0.5, 0.0) } else { (0.0, 0.5) };
    [cell_of(x - ox, y - oy), cell_of(x + ox, y + oy)]
}

// factorio: direction 0 = north, 4 = east (clockwise, 16 steps). the plane
// space is left-handed (z flipped), so clockwise on the map = POSITIVE yaw
fn dir_to_yaw(dir: u8) -> f32 {
    (dir as f32) * (TAU / 16.0)
}

fn orientation_to_yaw(o: f32) -> f32 {
    let sign = if tuning::ORIENT_FLIP.get() > 0.5 { -1.0 } else { 1.0 };
    sign * o * TAU
}

// live-tunable extra yaw for models whose authored facing is still unknown
fn key_extra_yaw(key: &str, oriented: bool) -> f32 {
    let deg = if key.contains("underground-belt") {
        tuning::UG_YAW.get()
    } else if key.contains("splitter") {
        tuning::SPLITTER_YAW.get()
    } else if key.contains("gun-turret/static2") {
        tuning::HEAD_YAW.get()
    } else if key.contains("VEHICLES/tank/") {
        tuning::TANK_YAW.get() // the tank model lies along z, unlike the trains
    } else if key.contains("VEHICLES/car/") {
        tuning::CAR_YAW.get() // the car export follows the tank convention
    } else if key.contains("ENEMIES/biters") {
        tuning::BITER_YAW.get() // biters: own facing offset, not the vehicle one
    } else if key.contains("ENTITIES/PLAYER") {
        tuning::PLAYER_YAW.get() // mixamo rig facing offset
    } else if oriented {
        tuning::VEHICLE_YAW.get() // trains/cars lie along x
    } else {
        0.0
    };
    deg.to_radians()
}

// models that ship mirrored and need an x-flip to match the game (live-toggle)
fn key_mirror(key: &str) -> bool {
    key.contains("pumpjack") && tuning::PUMPJACK_MIRROR.get() > 0.5
}

// cardinal index (0 N, 1 E, 2 S, 3 W) helpers for the connection logic
fn card(dir: u8) -> usize {
    ((dir / 4) & 3) as usize
}

const CARD_VEC: [(i32, i32); 4] = [(0, -1), (1, 0), (0, 1), (-1, 0)]; // N E S W

// which neighbor cells of `cell` hold the same group: bitmask N=1 E=2 S=4 W=8
fn neighbor_mask(grid: &Grid, cell: (i32, i32), kind: ConnKind) -> u8 {
    let mut mask = 0u8;
    for (i, v) in CARD_VEC.iter().enumerate() {
        if let Some((k, _)) = grid.get(&(cell.0 + v.0, cell.1 + v.1)) {
            if same_group(kind, *k) {
                mask |= 1 << i;
            }
        }
    }
    mask
}

// straight-run yaw for pipes/heat pipes: the models lie along z (north-
// south). returns None for elbows/junctions — no models for those yet
fn straight_yaw(mask: u8) -> Option<f32> {
    let ns = mask & 0b0101; // N|S
    let ew = mask & 0b1010; // E|W
    match (ns, ew) {
        (_, 0) => Some(0.0),      // north-south run (or isolated)
        (0, _) => Some(FRAC_PI_2), // east-west run
        _ => None,                // corner / T / cross
    }
}

// junction resolution: a dedicated corner.glb / t.glb / cross.glb from the
// model's folder wins WHEN IT EXISTS; otherwise the junction is COMPOSED
// from clipped halves of the straight model, one per connected side.
// corner assumed authored connecting north+east; t authored missing north
enum Junction {
    Straight(f32),            // full straight model at this yaw
    Model(&'static str, f32), // dedicated junction model
    Halves,                   // compose from per-direction half pieces
}

// which quarter-turn (0..3) a two-sided corner mask forms: N+E, E+S, S+W, W+N
fn corner_quarter(mask: u8) -> u8 {
    match mask {
        0b0011 => 0, // N+E
        0b0110 => 1, // E+S
        0b1100 => 2, // S+W
        _ => 3,      // W+N
    }
}

fn junction_variant(folder: &str, mask: u8) -> Junction {
    let opt = |n: &str| crate::models::optional_key(&format!("{folder}/{n}.glb"));
    match mask.count_ones() {
        0 | 1 => Junction::Straight(straight_yaw(mask).unwrap_or(0.0)),
        2 => {
            if let Some(yaw) = straight_yaw(mask) {
                return Junction::Straight(yaw); // opposite pair
            }
            match opt("corner") {
                Some(k) => Junction::Model(k, corner_quarter(mask) as f32 * FRAC_PI_2),
                None => Junction::Halves,
            }
        }
        3 => {
            let quarter = match (!mask) & 0xF {
                0b0001 => 0.0, // missing north
                0b0010 => 1.0, // missing east
                0b0100 => 2.0, // missing south
                _ => 3.0,      // missing west
            };
            match opt("t") {
                Some(k) => Junction::Model(k, quarter * FRAC_PI_2),
                None => Junction::Halves,
            }
        }
        _ => match opt("cross") {
            Some(k) => Junction::Model(k, 0.0),
            None => Junction::Halves,
        },
    }
}

// belt curve: feeders are neighbor belts pointing into this cell.
// returns (curved, turns_left)
fn belt_curve(grid: &Grid, cell: (i32, i32), dir: u8) -> (bool, bool) {
    let out = card(dir);
    let mut behind = false;
    let mut side: Option<usize> = None;
    for (k, v) in CARD_VEC.iter().enumerate() {
        // a belt at C feeds this cell when it points at us (its dir == k
        // looking from C toward us => C = cell - v(k))
        let c = (cell.0 - v.0, cell.1 - v.1);
        if let Some((kind, d)) = grid.get(&c) {
            if matches!(*kind, ConnKind::Belt | ConnKind::Under | ConnKind::Splitter)
                && card(*d) == k
            {
                if k == out {
                    behind = true;
                } else if k != (out + 2) % 4 {
                    side = Some(k);
                }
            }
        }
    }
    match (behind, side) {
        (false, Some(k)) => {
            // one side feeder, nothing from behind: curved.
            // left turn when the output is a ccw step from the feed direction
            let left = out == (k + 3) % 4;
            (true, left)
        }
        _ => (false, false),
    }
}

// --- per-frame instance snapshot -------------------------------------------------------------

// one placed model piece of a connectable entity (junctions can be several)
struct Piece {
    key: &'static str,
    yaw: f32,
    roll: f32,
    lift: f32,
    mirror: bool,
    clip: [f32; 2], // plane-space clip direction (composed junction halves)
    off: [f32; 2],  // map-space placement offset
}

impl Piece {
    fn new(key: &'static str, yaw: f32) -> Self {
        Self { key, yaw, roll: 0.0, lift: 0.0, mirror: false, clip: [0.0; 2], off: [0.0; 2] }
    }
}

// pipe / heat-pipe / wall: pick the junction variant from the neighbor mask
fn junction_pieces(e: &Entry, grid: &Grid, cell: (i32, i32)) -> Vec<Piece> {
    let mask = neighbor_mask(grid, cell, e.model.kind);
    let straight = e.model.parts[0].key;
    // wall model lies along x, pipes along z
    let (folder, axis_off, extra_yaw) = match e.model.kind {
        ConnKind::Pipe => ("ENTITIES/STRUCTURES/pipes", 0.0, tuning::PIPE_YAW.get()),
        ConnKind::Heat => ("ENTITIES/STRUCTURES/heat-pipe", 0.0, tuning::PIPE_YAW.get()),
        _ => ("ENTITIES/STRUCTURES/stone-wall", FRAC_PI_2, tuning::WALL_YAW.get()),
    };
    let extra_yaw = extra_yaw.to_radians();
    // pipes sit centered on the ground plane — lift them
    // (heat pipes hug the ground, own knob)
    let lift = match e.model.kind {
        ConnKind::Wall => 0.0,
        ConnKind::Heat => tuning::HEAT_LIFT.get(),
        _ => tuning::PIPE_LIFT.get(),
    };
    let mut pieces = Vec::new();
    match junction_variant(folder, mask) {
        Junction::Straight(yaw) => {
            pieces.push(Piece { lift, ..Piece::new(straight, yaw + axis_off + extra_yaw) });
        }
        Junction::Model(key, yaw) => {
            pieces.push(Piece { lift, ..Piece::new(key, yaw + extra_yaw) });
        }
        // pipe corners: reuse the curved pipe-to-ground model, rolled onto
        // its side so the down-bend turns sideways
        Junction::Halves if e.model.kind == ConnKind::Pipe && mask.count_ones() == 2 => {
            let quarter = corner_quarter(mask);
            // tile-centering offset, calibrated for the N+E corner and rotated
            // with the piece (map space: one clockwise quarter is (x,y) -> (-y,x))
            let mut off = [tuning::CORNER_DX.get(), tuning::CORNER_DY.get()];
            for _ in 0..quarter {
                off = [-off[1], off[0]];
            }
            pieces.push(Piece {
                roll: tuning::CORNER_ROLL.get().to_radians(),
                lift: tuning::CORNER_LIFT.get(),
                off,
                ..Piece::new(
                    "ENTITIES/STRUCTURES/pipes/pipe-to-ground.glb",
                    quarter as f32 * FRAC_PI_2 + tuning::CORNER_YAW.get().to_radians() + extra_yaw,
                )
            });
        }
        Junction::Halves => {
            for (i, v) in CARD_VEC.iter().enumerate() {
                if mask & (1 << i) == 0 {
                    continue;
                }
                let axis = if i % 2 == 0 { 0.0 } else { FRAC_PI_2 } + axis_off + extra_yaw;
                // plane space: x = east, z = north (map -y)
                let clip = [v.0 as f32, -(v.1 as f32)];
                pieces.push(Piece { lift, clip, ..Piece::new(straight, axis) });
            }
        }
    }
    pieces
}

// underground belt: entrance or exit? the PARTNER underground sits ahead of
// an entrance and behind an exit (same direction, within reach). fall back
// to belt neighbors for lone halves
fn underground_yaw(e: &Entry, grid: &Grid, cell: (i32, i32)) -> f32 {
    let out = card(e.dir);
    let vin = CARD_VEC[out];
    let partner_at = |sign: i32| {
        (1..=9).any(|i| {
            grid.get(&(cell.0 + vin.0 * i * sign, cell.1 + vin.1 * i * sign))
                .is_some_and(|(k, d)| *k == ConnKind::Under && card(*d) == out)
        })
    };
    let exit = if partner_at(1) {
        false // partner ahead -> entrance
    } else if partner_at(-1) {
        true // partner behind -> exit
    } else {
        // no partner: continues into a belt ahead = exit
        grid.get(&(cell.0 + vin.0, cell.1 + vin.1))
            .is_some_and(|(k, d)| same_group(ConnKind::Belt, *k) && card(*d) == out)
    };
    dir_to_yaw(e.dir) + tuning::UG_YAW.get().to_radians() + if exit { PI } else { 0.0 }
}

// connectable entities (belts, pipes, walls...) pick their model variant +
// rotation from the neighbor grid every frame
fn connectable_instances(
    e: &Entry,
    grid: &Grid,
    belt_uv: [f32; 2],
    splitter_uv: [f32; 2],
    item_cells: &FxHashSet<(i32, i32)>,
    out: &mut Vec<Instance>,
) {
    let cell = cell_of(e.x, e.y);
    let mut uv_scroll = [0.0f32; 2];
    // single-piece kinds (belts — the bulk at every zoom) avoid the Vec: a
    // heap alloc per belt per frame was measurable on full-base views
    let mut single: Option<Piece> = None;
    let mut multi: Vec<Piece> = Vec::new(); // pipe/heat/wall junctions only
    match e.model.kind {
        ConnKind::Pipe | ConnKind::Heat | ConnKind::Wall => {
            multi = junction_pieces(e, grid, cell);
        }
        ConnKind::Belt => {
            let (curved, left) = belt_curve(grid, cell, e.dir);
            uv_scroll = belt_uv; // treads scroll on the top faces
            let key = if curved {
                crate::models::belt_corner_key(e.model.parts[0].key)
            } else {
                e.model.parts[0].key
            };
            let extra =
                if curved { tuning::BELT_L_YAW.get() } else { tuning::BELT_I_YAW.get() };
            let flip = tuning::BELT_L_MIRROR.get() > 0.5;
            single = Some(Piece {
                mirror: curved && (left ^ flip),
                ..Piece::new(key, dir_to_yaw(e.dir) + extra.to_radians())
            });
        }
        ConnKind::Under => {
            single = Some(Piece::new(e.model.parts[0].key, underground_yaw(e, grid, cell)));
        }
        // the gate: fixed model, yaw from its own direction (0 = the
        // north-south passage, 4 = east-west); the shapekey does the motion
        ConnKind::Gate => {
            single = Some(Piece::new(
                e.model.parts[0].key,
                dir_to_yaw(e.dir) + tuning::GATE_YAW.get().to_radians(),
            ));
        }
        // own model; scroll its belt surface only while an item rides it
        // (the moving treads ARE the "items passing" animation)
        ConnKind::Splitter => {
            let busy = splitter_cells(e.x, e.y, e.dir).iter().any(|c| item_cells.contains(c));
            if busy {
                uv_scroll = splitter_uv;
            }
            single = Some(Piece::new(
                e.model.parts[0].key,
                dir_to_yaw(e.dir) + tuning::SPLITTER_YAW.get().to_radians(),
            ));
        }
        // PipeEnd (pipe-to-ground)
        _ => {
            single = Some(Piece::new(
                e.model.parts[0].key,
                dir_to_yaw(e.dir) + tuning::PTG_YAW.get().to_radians(),
            ));
        }
    }
    // splitters play their glb animation while items pass through
    let anim_t = if e.model.kind == ConnKind::Splitter { e.anim_t } else { 0.0 };
    for p in single.into_iter().chain(multi) {
        out.push(Instance {
            key: p.key,
            scale_ref: p.key,
            tiles: e.model.tiles,
            x: e.x + p.off[0],
            y: e.y + p.off[1],
            lift: p.lift,
            yaw: p.yaw + tuning::model_yaw_extra(p.key),
            mirror: p.mirror,
            uv_scroll,
            clip_dir: p.clip,
            roll: p.roll,
            anim_t,
            morph: e.morph.is_finite().then(|| e.morph.clamp(0.0, 1.0)),
            ..Instance::default()
        });
    }
}

// advance the glb animation while the entity is "working" (fingerprint
// changed recently). inserters instead map the real arm's live position
// straight onto the clip
fn advance_anim(e: &mut Entry, frame: u64, dt: f32) {
    let duration = e
        .model
        .parts
        .iter()
        .filter_map(|p| crate::models::ready_duration(p.key))
        .fold(0.0f32, f32::max);
    let working = e.last_change_frame != 0
        && frame.saturating_sub(e.last_change_frame) <= crate::settings::WORKING_WINDOW_FRAMES;
    let range = tuning::INSERTER_ARM_RANGE.get();
    if e.arm_angle.is_finite() && duration > 0.001 && range > 0.0001 {
        // the arm clip is a symmetric out-and-back (the "spin" node sweeps
        // 0->peak->0 over [0,duration], peak at the midpoint). map the real
        // arm's ABSOLUTE deviation from its rest/facing orientation straight
        // onto the clip — no accumulator, so the mesh matches the arm's live
        // position with NO lag (the held item follows the same signal).
        // extending plays the first half (rest->peak), retracting the second
        let rest = e.dir as f32 / 16.0;
        let dev = |a: f32| {
            let mut d = (a - rest).rem_euclid(1.0);
            if d > 0.5 {
                d = 1.0 - d;
            }
            d
        };
        let cur = dev(e.arm_angle);
        let prev = if e.prev_arm_angle.is_finite() { dev(e.prev_arm_angle) } else { cur };
        // which side of its facing is the real arm on? the model clip only
        // swings one way, so mirror it when the arm is on the negative side.
        // hysteresis: only decide when the arm has meaningfully deviated, so
        // it doesn't flip-flicker while parked near the facing axis
        let signed = (e.arm_angle - rest + 0.5).rem_euclid(1.0) - 0.5;
        if signed.abs() > 0.03 {
            let flip = tuning::INSERTER_SWING_FLIP.get() > 0.5;
            e.swing_mirror = (signed < 0.0) ^ flip;
        }
        // idle: leave the pose put (avoids a half-flip jump when the arm
        // stops mid-stroke); only re-place when it's actually moving
        if (cur - prev).abs() > 1e-5 {
            let frac = (cur / range).clamp(0.0, 1.0);
            let half = duration * 0.5;
            e.anim_t = if cur > prev { frac * half } else { duration - frac * half };
        }
        e.prev_arm_angle = e.arm_angle;
    } else if working && duration > 0.001 {
        let rate = tuning::ANIM_SPEED.get() * e.anim_rate;
        e.anim_t = (e.anim_t + dt * rate) % duration;
    } else {
        // stopped: snap back to the rest pose
        e.anim_t = 0.0;
    }
}

// tank tracks: accumulate signed tiles driven (negative in reverse); when
// turning in place the chains counter-rotate
fn update_track_phase(e: &mut Entry) {
    if e.orientation.is_finite() {
        let (dx, dy) = (e.x - e.prev_x, e.y - e.prev_y);
        let dist = (dx * dx + dy * dy).sqrt();
        // ignore teleports (respawn / editor moves)
        if dist > 0.002 && dist < 5.0 {
            let a = e.orientation * TAU;
            // orientation 0 = north = -y on the map, clockwise
            let forward = (a.sin(), -a.cos());
            let sign = if dx * forward.0 + dy * forward.1 < 0.0 { -1.0 } else { 1.0 };
            e.track_phase += sign * dist;
            e.track_phase_b += sign * dist;
        } else if e.prev_orient.is_finite() {
            // turning in place: only when NOT driving — regular motion covers
            // the moving case
            let mut d = e.orientation - e.prev_orient;
            if d > 0.5 {
                d -= 1.0;
            } else if d < -0.5 {
                d += 1.0;
            }
            let arc = d * TAU * tuning::TANK_TURN_GAUGE.get();
            e.track_phase += arc;
            e.track_phase_b -= arc;
        }
        e.prev_orient = e.orientation;
    }
    e.prev_x = e.x;
    e.prev_y = e.y;
}

// silo rocket: base sink so only the head shows, an emerge animation over the
// first ~1.5s (rocket built = rises up), and the LAUNCH — the game flies the
// rocket north (y drops); we turn that northward travel into upward lift and
// pin it at the silo, so it rises vertically at the game's real speed instead
// of sliding off-screen
fn rocket_lift(e: &Entry, frame: u64) -> Option<f32> {
    if !e.rocket_flight.is_finite() {
        return None;
    }
    let base = tuning::ROCKET_BASE_LIFT.get();
    let secs = tuning::ROCKET_EMERGE_SECS.get().max(0.05);
    let drop = tuning::ROCKET_EMERGE_DROP.get();
    let age = frame.saturating_sub(e.first_seen_frame) as f32 / 60.0;
    let emerge = -drop * (1.0 - (age / secs).clamp(0.0, 1.0));
    let flight = (e.first_y - e.y).max(0.0) * tuning::ROCKET_RISE_SCALE.get();
    Some(base + emerge + flight)
}

// player states — indexes into models::PLAYER_KEYS
const PS_IDLE: u8 = 0;
const PS_RUN: u8 = 1;
const PS_SHOOT_RUN: u8 = 2;
const PS_SHOOT_STAND: u8 = 3;
const PS_MINING: u8 = 4;

// per-frame player update: movement detection, render-position smoothing,
// the pose state machine, clip advance, and the stand-on-belt lift
fn update_player(e: &mut Entry, frame: u64, dt: f32, grid: &Grid) {
    // movement: the raw position field steps at 60UPS, so "moving" is a
    // change within the last few frames — per-frame deltas would flicker
    // idle/run at fps > 60
    let (dx, dy) = (e.x - e.prev_x, e.y - e.prev_y);
    let dist = (dx * dx + dy * dy).sqrt();
    e.move_accum += dt;
    if dist > 1e-4 && dist < 5.0 {
        e.last_move_frame = frame;
        let speed = dist / e.move_accum.max(1e-3);
        e.speed_ema += (speed - e.speed_ema) * 0.25;
        e.move_accum = 0.0;
    }
    e.prev_x = e.x;
    e.prev_y = e.y;

    // ema toward the raw position: the stepped field judders against the
    // game's interpolated camera (the player sits at screen center, it shows).
    // 1.0 = raw; teleports snap
    let k = tuning::PLAYER_SMOOTH.get().clamp(0.05, 1.0);
    e.smooth_x += (e.x - e.smooth_x) * k;
    e.smooth_y += (e.y - e.smooth_y) * k;
    if (e.x - e.smooth_x).abs() + (e.y - e.smooth_y).abs() > 3.0 {
        e.smooth_x = e.x;
        e.smooth_y = e.y;
    }

    // pose priority: mining > shooting (run/stand variants) > running > idle
    let moving = frame.saturating_sub(e.last_move_frame) <= 8;
    let mining = {
        let mf = crate::hooks::player::mining_frame();
        mf != 0 && frame.saturating_sub(mf) <= tuning::PLAYER_MINE_WINDOW.get().max(1.0) as u64
    };
    let shooting = e.last_shot_frame != 0
        && frame.saturating_sub(e.last_shot_frame)
            <= tuning::PLAYER_SHOOT_WINDOW.get().max(1.0) as u64;
    let state = if mining {
        PS_MINING
    } else if shooting && moving {
        PS_SHOOT_RUN
    } else if shooting {
        PS_SHOOT_STAND
    } else if moving {
        PS_RUN
    } else {
        PS_IDLE
    };
    if state != e.player_state {
        e.player_state = state;
        e.anim_t = 0.0; // swing/burst clips read from their first frame
    }

    // advance the state's clip; run clips pace to the real ground speed
    // (exoskeletons), the rest play at authored speed
    let duration =
        crate::models::ready_duration(crate::models::player_key(state)).unwrap_or(0.0);
    if duration > 0.001 {
        let mut rate = tuning::PLAYER_ANIM_SPEED.get();
        if state == PS_RUN || state == PS_SHOOT_RUN {
            rate *= (e.speed_ema / tuning::PLAYER_RUN_REF.get().max(0.1)).clamp(0.5, 3.0);
        }
        e.anim_t = (e.anim_t + dt * rate) % duration;
    }

    // stand ON belts instead of clipping through: while the cell under the
    // player is a belt / underground / splitter, ride at the belt top height
    let on_belt = matches!(
        grid.get(&cell_of(e.x, e.y)),
        Some((ConnKind::Belt | ConnKind::Under | ConnKind::Splitter, _))
    );
    let target = if on_belt { tuning::PLAYER_BELT_LIFT.get() } else { 0.0 };
    e.player_lift += (target - e.player_lift) * 0.25;
}

// radar dish: keep turning while the entity keeps redrawing (a powered
// radar's vanilla animation re-queues its chunk every frame)
fn advance_spin(e: &mut Entry, frame: u64, dt: f32) {
    if e.model.parts.iter().any(|p| p.yaw == YawSource::Spin)
        && frame.saturating_sub(e.last_change_frame) < 15
    {
        e.spin_yaw =
            (e.spin_yaw + dt * tuning::RADAR_SPIN.get().to_radians()).rem_euclid(TAU);
    }
}

fn is_robot(e: &Entry) -> bool {
    let k = e.model.parts[0].key;
    k.contains("construction-robot") || k.contains("logistic-robot")
}

// flying robots: their draws (and so their recorded positions) can arrive as
// rarely as once a second, which looked like 1 fps movement. dead-reckon
// along the measured velocity, capped at one record interval so a robot that
// stopped doesn't sail on, and glide over the correction at each new record
fn update_robot(e: &mut Entry, frame: u64) {
    let ahead = (frame.saturating_sub(e.pos_frame) as f32).min(e.rec_interval);
    let (tx, ty) = (e.x + e.vel_x * ahead, e.y + e.vel_y * ahead);
    if (tx - e.smooth_x).abs() + (ty - e.smooth_y).abs() > 3.0 {
        e.smooth_x = tx;
        e.smooth_y = ty;
    } else {
        e.smooth_x += (tx - e.smooth_x) * 0.3;
        e.smooth_y += (ty - e.smooth_y) * 0.3;
    }
}

// regular (non-connectable) entity: one instance per model part
fn entity_instances(e: &Entry, frame: u64, out: &mut Vec<Instance>) {
    let scale_ref = e.model.parts[0].key;
    let rocket_lift = rocket_lift(e, frame);
    // flying robots have no altitude field in our record — the game draws
    // them aloft, but we replaced that with a ground-pinned model. hover
    // them at a fixed height so they read as flying again
    let robot_lift = is_robot(e).then(|| tuning::ROBOT_LIFT.get());
    // relative turret orientation -> node-space yaw. the node rotation
    // sits inside the z-flip, which mirrors the spin direction — hence
    // the leading minus (TANK_TURRET_FLIP reverses it if that's wrong)
    let turret_yaw = if e.turret.is_finite() {
        let flip = if tuning::TANK_TURRET_FLIP.get() > 0.5 { -1.0 } else { 1.0 };
        -orientation_to_yaw(e.turret) * flip
    } else {
        0.0
    };
    for part in &e.model.parts {
        // the player: swap the glb by pose state, place at the smoothed
        // position, ride the belt lift, and tint with the /color player color
        let key = if e.is_player { crate::models::player_key(e.player_state) } else { part.key };
        let oriented = e.orientation.is_finite();
        // the entity's facing alone — extra part placements rotate with it
        let base_yaw = match part.yaw {
            // vehicles have a live orientation; buildings only a direction
            YawSource::Direction if oriented => orientation_to_yaw(e.orientation),
            YawSource::Direction => dir_to_yaw(e.dir),
            YawSource::Orientation if oriented => orientation_to_yaw(e.orientation),
            YawSource::Spin => e.spin_yaw,
            _ => 0.0,
        };
        let yaw = base_yaw + key_extra_yaw(key, oriented && part.yaw == YawSource::Direction);
        // the rocket stays pinned at the silo; everything else at its live position
        let (x, y) = if rocket_lift.is_some() {
            (e.first_x, e.first_y)
        } else if e.is_player || robot_lift.is_some() {
            (e.smooth_x, e.smooth_y)
        } else {
            (e.x, e.y)
        };
        let tiles = if e.is_player { tuning::PLAYER_SIZE.get() } else { e.model.tiles };
        let tint = if e.is_player && e.player_color[3] > 0.0 {
            // tint strength mixes the color toward white; a=1 arms the tint
            let s = tuning::PLAYER_TINT.get().clamp(0.0, 1.0);
            let mix = |c: f32| 1.0 + (c.clamp(0.0, 1.0) - 1.0) * s;
            [mix(e.player_color[0]), mix(e.player_color[1]), mix(e.player_color[2]), 1.0]
        } else {
            [0.0; 4]
        };
        // per-model placement nudge from f3dm_tuning.txt ("off:name=dx,dy,dz")
        let poff = tuning::model_offset_extra(key);
        let inst = Instance {
            key,
            scale_ref,
            tiles,
            // a stale flag counts as idle: no redraw = no animation = no arcs
            active: e.active && frame.saturating_sub(e.active_frame) < 30,
            x: x + poff[0],
            y: y + poff[1],
            lift: rocket_lift.or(robot_lift).unwrap_or(0.0) + poff[2] + e.player_lift,
            yaw: yaw + tuning::model_yaw_extra(key),
            // inserters mirror per facing so the arm swings the right way;
            // pumpjacks ship mirrored and get flipped back
            mirror: e.swing_mirror || key_mirror(key),
            anim_t: e.anim_t,
            turret_yaw,
            track_phase: e.track_phase,
            track_phase_b: e.track_phase_b,
            tint,
            morph: e.morph.is_finite().then(|| e.morph.clamp(0.0, 1.0)),
            ..Instance::default()
        };
        // "copy:name=dx,dy,dz[,yaw]" tuning lines: draw this part again at an
        // entity-relative offset, rotated with the facing — the centrifuge's
        // one animated column rig fills its three sockets this way
        for c in tuning::model_copies(key).iter() {
            let (s, co) = base_yaw.sin_cos(); // map yaw: positive = clockwise
            out.push(Instance {
                x: inst.x + c[0] * co - c[1] * s,
                y: inst.y + c[0] * s + c[1] * co,
                lift: inst.lift + c[2],
                yaw: inst.yaw + c[3].to_radians(),
                ..inst.clone()
            });
        }
        out.push(inst);
    }
}

// belt tread scroll: a global uv phase (wraps at 1, sampler repeats)
fn belt_uv_phase(dt: f32) -> [f32; 2] {
    static CLOCK: AtomicU64 = AtomicU64::new(0);
    let clock = f64::from_bits(CLOCK.load(Ordering::Relaxed)) + dt as f64;
    CLOCK.store(clock.to_bits(), Ordering::Relaxed);
    let phase = ((clock * tuning::BELT_SPEED.get() as f64).rem_euclid(1.0)) as f32;
    if tuning::BELT_UV_U.get() > 0.5 { [phase, 0.0] } else { [0.0, phase] }
}

// promote this frame's recorded non-belt item batch (inserter hands, machine
// i/o) to the live set; persist the last batch across frames that recorded
// nothing (draws can skip a render frame at fps > 60)
fn promote_item_batch(frame: u64) {
    let batch = std::mem::take(&mut *plock(&ITEM_BUF));
    if !batch.is_empty() {
        *plock(&LIVE_ITEMS) = batch;
        LIVE_FRAME.store(frame, Ordering::Relaxed);
    } else {
        // hold briefly, but clear fast enough that items vanish promptly
        // when their source (inserter...) is removed
        let hold = tuning::ITEM_CLEAR_FRAMES.get().max(1.0) as u64;
        if frame.saturating_sub(LIVE_FRAME.load(Ordering::Relaxed)) > hold {
            plock(&LIVE_ITEMS).clear();
        }
    }
}

// the persistent live item set (belts, inserter hands, machine i/o) — the
// vanilla sprite still draws under each, hidden by the lifted model
fn item_instances(frame: u64, cull: &ViewCull, out: &mut Vec<Instance>) -> usize {
    let def_size = tuning::ITEM_SIZE.get();
    let def_lift = tuning::ITEM_LIFT.get();
    let mut push = |it: &ItemDraw| {
        if !cull.contains(it.x, it.y) {
            return;
        }
        let lift = if it.lift.is_nan() { def_lift } else { it.lift };
        let size = if it.size.is_nan() || it.size <= 0.0 { def_size } else { it.size };
        out.push(Instance {
            key: it.key,
            scale_ref: it.key,
            tiles: size,
            x: it.x,
            y: it.y,
            lift,
            ..Instance::default()
        });
    };
    let mut n = 0usize;
    {
        let items = plock(&LIVE_ITEMS);
        items.iter().for_each(&mut push);
        n += items.len();
    }
    // belt entries expire when the belt hasn't drawn its items for a while
    // (scrolled off-screen, or the belt emptied without a final drawItems)
    let hold = tuning::ITEM_CLEAR_FRAMES.get().max(1.0) as u64;
    let mut belts = plock(&BELT_ITEMS);
    belts.retain(|_, (f, _)| frame.saturating_sub(*f) <= hold);
    for (_, items) in belts.values() {
        items.iter().for_each(&mut push);
        n += items.len();
    }
    n
}

// space-age platform views record entities with coordinates that overlap
// the main surface — only the dominant surface renders. full-registry scan,
// so recomputed every ~half second (the answer changes on surface switches,
// which are far rarer than frames)
fn dominant_surface(frame: u64, map: &FxHashMap<usize, Entry>) -> usize {
    use std::sync::atomic::AtomicUsize;
    static CACHED: AtomicUsize = AtomicUsize::new(0);
    static CACHE_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    let last = CACHE_FRAME.load(Ordering::Relaxed);
    if last != u64::MAX && frame.saturating_sub(last) < 30 {
        return CACHED.load(Ordering::Relaxed);
    }
    let mut counts: HashMap<usize, u32> = HashMap::new();
    for e in map.values() {
        if e.surface != 0 {
            *counts.entry(e.surface).or_default() += 1;
        }
    }
    let dom = counts.iter().max_by_key(|(_, c)| **c).map(|(s, _)| *s).unwrap_or(0);
    CACHED.store(dom, Ordering::Relaxed);
    CACHE_FRAME.store(frame, Ordering::Relaxed);
    dom
}

// view-rect culling for the per-frame snapshot: per-entity work (animation,
// junction lookups, instance building) only runs for entities near the
// screen. margin matches the renderer's own on_screen slack (20% of the
// span) plus a few tiles so wide models poking in from the edge still draw
struct ViewCull {
    active: bool,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
}

impl ViewCull {
    fn new(rect: (f32, f32, f32, f32)) -> Self {
        let (l, t, sx, sy) = rect;
        let (mx, my) = (sx * 0.2 + 6.0, sy * 0.2 + 6.0);
        Self {
            active: sx > 0.5 && sy > 0.5,
            x0: l - mx,
            x1: l + sx + mx,
            y0: t - my,
            y1: t + sy + my,
        }
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        !self.active || (x > self.x0 && x < self.x1 && y > self.y0 && y < self.y1)
    }
}

// advance animations and snapshot the part instances for this frame.
// durations come from the loaded models (0 while still loading)
pub fn tick(dt: f32, view_rect: (f32, f32, f32, f32)) -> Vec<Instance> {
    let frame = crate::hooks::frame::frame_count();
    let cull = ViewCull::new(view_rect);
    let mut map = plock(&ENTITIES);
    let grid = plock(&GRID);
    // capacity from the previous frame's culled count, not the whole registry
    static LAST_LEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let mut out = Vec::with_capacity(LAST_LEN.load(Ordering::Relaxed) + 64);

    let belt_uv = belt_uv_phase(dt);
    let splitter_uv = if tuning::SPLITTER_UV_FLIP.get() > 0.5 {
        [-belt_uv[0], -belt_uv[1]]
    } else {
        belt_uv
    };
    promote_item_batch(frame);
    let item_cells: FxHashSet<(i32, i32)> = {
        let mut cells: FxHashSet<(i32, i32)> =
            plock(&LIVE_ITEMS).iter().map(|it| cell_of(it.x, it.y)).collect();
        for (_, items) in plock(&BELT_ITEMS).values() {
            cells.extend(items.iter().map(|it| cell_of(it.x, it.y)));
        }
        cells
    };
    let main_surface = dominant_surface(frame, &map);
    // refresh the live darkness for the shader's day/night sync
    crate::hooks::daynight::sample(main_surface);

    for e in map.values_mut() {
        if !e.show || (e.surface != 0 && main_surface != 0 && e.surface != main_surface) {
            continue;
        }
        // off-screen: skip ALL per-entity work (nothing of it is visible;
        // animation state simply pauses until the entity scrolls back in)
        if !cull.contains(e.x, e.y) {
            continue;
        }
        if e.is_player {
            // unlike buildings, a character that stops re-recording is HIDDEN
            // (entered a vehicle, remote view...) — expire it right away.
            // the frame-counter fingerprint updates last_change_frame every draw
            let expire = tuning::PLAYER_EXPIRE_FRAMES.get().max(2.0) as u64;
            if frame.saturating_sub(e.last_change_frame) > expire {
                continue;
            }
            update_player(e, frame, dt, &grid);
            entity_instances(e, frame, &mut out);
            continue;
        }
        advance_anim(e, frame, dt);
        if e.model.kind != ConnKind::None {
            connectable_instances(e, &grid, belt_uv, splitter_uv, &item_cells, &mut out);
        } else {
            if is_robot(e) {
                update_robot(e, frame);
            }
            advance_spin(e, frame, dt);
            update_track_phase(e);
            entity_instances(e, frame, &mut out);
        }
    }

    // release the registry + grid before the item/ghost passes: these are the
    // locks every parallel draw hook contends on — hold them no longer than
    // the entity loop needs
    let n_entities = map.len();
    drop(grid);
    drop(map);

    ghost_instances(frame, &mut out);
    let n_items = item_instances(frame, &cull, &mut out);
    LAST_LEN.store(out.len(), Ordering::Relaxed);
    if frame % 300 == 0 {
        log::info!(
            "[entities] tick: {} entities, {} instances, {} live items, {} record_item calls total",
            n_entities,
            out.len(),
            n_items,
            RECORD_ITEM_N.load(Ordering::Relaxed),
        );
    }
    out
}

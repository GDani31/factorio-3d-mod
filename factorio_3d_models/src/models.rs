// model registry: maps factorio prototype names to glb files under models/
// (a copy of the FUE5 export tree) and loads them lazily on a worker thread.
//
// lookup rules for a prototype name:
// - exact folder-name match (ENTITIES/STRUCTURES > VEHICLES > RESOURCES > FOLIAGE)
// - tier suffix stripped: assembling-machine-3 -> assembling-machine
// - alias table for the odd ones (chests, turret parts, belts)
//
// a folder's glb: animated*.glb preferred over static*.glb.
// until a model finishes loading, entities keep their vanilla sprite.

use crate::gltf_model::ModelData;
use crate::util::memo;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

// how an entity model part rotates with the entity
#[derive(Clone, Copy, PartialEq)]
pub enum YawSource {
    Direction,   // building direction (N/E/S/W)
    Orientation, // smooth 0..1 orientation (turret head)
    Fixed,       // never rotates (turret base)
    Spin,        // continuous rotation while working (radar dish)
}

#[derive(Clone)]
pub struct Part {
    pub key: &'static str, // interned relative glb path — identity for gpu cache
    pub yaw: YawSource,
}

// how an entity connects to its neighbors (drives model variant + rotation)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnKind {
    None,
    Belt,     // belt-I / belt-L by curve state
    Under,    // underground belt: entrance/exit inferred from neighbors
    Splitter, // own model, but occupies TWO belt-grid cells for neighbors
    Pipe,     // pipe-I when the run is straight (junctions: straight stand-in)
    PipeEnd,  // pipe-to-ground: fixed model, still counts as a pipe neighbor
    Heat,     // heat pipes, same straight-run logic in their own group
    Wall,     // one segment model, yaw follows the connection axis
    Gate,     // own model + shapekey, counts as a wall neighbor
}

// an entity prototype's full model: usually one part, turrets have two
#[derive(Clone)]
pub struct EntityModel {
    pub parts: Vec<Part>,
    // entity footprint in tiles (widest axis) — the model's xz extent gets
    // scaled to cover it; all parts share part[0]'s scale factor
    pub tiles: f32,
    pub kind: ConnKind,
}

// the corner model matching a straight belt key — derived from the belt's own
// belt-I key so tier variants (belts/fast/...) keep their color on curves.
// keyed by the interned key's POINTER: this runs per curved belt per frame,
// a string-keyed memo was an alloc + string hash each call
pub fn belt_corner_key(straight: &'static str) -> &'static str {
    static CACHE: LazyLock<Mutex<crate::util::FxHashMap<usize, &'static str>>> =
        LazyLock::new(Default::default);
    let mut cache = CACHE.lock().unwrap();
    *cache
        .entry(straight.as_ptr() as usize)
        .or_insert_with(|| intern(straight.replace("belt-I.glb", "belt-L.glb")))
}

// player character states -> per-state glbs (one clip per file, same rig).
// order matches the PS_* constants in entities.rs
pub const PLAYER_KEYS: [&str; 5] = [
    "ENTITIES/PLAYER/idle.glb",
    "ENTITIES/PLAYER/running.glb",
    "ENTITIES/PLAYER/shoot-run.glb",
    "ENTITIES/PLAYER/shoot-stand.glb",
    "ENTITIES/PLAYER/mining.glb",
];

pub fn player_key(state: u8) -> &'static str {
    PLAYER_KEYS[(state as usize).min(PLAYER_KEYS.len() - 1)]
}

pub fn is_player_key(key: &str) -> bool {
    key.starts_with("ENTITIES/PLAYER/")
}

// the spidertron model drives a torso-yaw + 8-leg IK rig (spider_instances)
pub fn is_spider_key(key: &str) -> bool {
    key.starts_with("ENTITIES/VEHICLES/spidertron")
}

// optional variant glb (corner.glb / t.glb / cross.glb next to a straight
// model): interned key when the file exists, memoized
pub fn optional_key(rel: &str) -> Option<&'static str> {
    static CACHE: LazyLock<Mutex<HashMap<String, Option<&'static str>>>> =
        LazyLock::new(Default::default);
    let reg = REGISTRY.get()?.as_ref()?;
    memo(&CACHE, rel.to_string(), || {
        reg.root.join(rel).exists().then(|| intern(rel.to_string()))
    })
}

// vanilla footprints (widest axis) for the prototypes we replace
fn footprint(name: &str) -> f32 {
    match name {
        "assembling-machine-1" | "assembling-machine-2" | "assembling-machine-3"
        | "chemical-plant" | "centrifuge" | "lab" | "electric-furnace" | "radar"
        | "electric-mining-drill" | "pumpjack" | "beacon" | "solar-panel"
        | "storage-tank" | "boiler" | "heat-exchanger" => 3.0,
        "oil-refinery" | "nuclear-reactor" | "steam-engine" | "steam-turbine" => 5.0,
        "tank" => 4.0,
        "car" => 2.0,
        "rocket-silo" => 9.0,
        "rocket-silo-rocket" => 4.0,
        "roboport" => 4.0,
        "locomotive" | "cargo-wagon" | "fluid-wagon" => 6.0,
        "stone-furnace" | "steel-furnace" | "gun-turret" | "burner-mining-drill"
        | "accumulator" | "substation" | "big-electric-pole" | "train-stop"
        | "decider-combinator" | "arithmetic-combinator" | "pump" | "offshore-pump" => 2.0,
        _ => 1.0,
    }
}

pub enum LoadState {
    Loading,
    Ready(Arc<ModelData>),
    Failed,
}

struct Registry {
    root: PathBuf,
    // folder name -> relative glb paths (static housing first, then the
    // animated rigs — they LAYER: one entity draws all of them)
    by_name: HashMap<String, Vec<String>>,
    // item name -> relative glb path (ENTITIES/ITEMS, for items on belts)
    items_by_name: HashMap<String, String>,
    // resolved prototype name -> entity model (memoized, None = no model)
    resolved: Mutex<HashMap<String, Option<EntityModel>>>,
    loaded: Mutex<HashMap<&'static str, LoadState>>,
    tx: Sender<&'static str>,
}

static REGISTRY: OnceLock<Option<Registry>> = OnceLock::new();
static SCAN_LOGGED: AtomicBool = AtomicBool::new(false);

// leak the key string once — parts/gpu-cache hold &'static str forever
fn intern(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

pub fn init() {
    REGISTRY.get_or_init(|| match build_registry() {
        Ok(r) => Some(r),
        Err(e) => {
            log::error!("[models] registry init failed: {e:#} — 3d models disabled");
            None
        }
    });
    // connectables draw only once per chunk re-queue, so their models must
    // be ready BEFORE the first draw or they stay vanilla until a re-render
    let mut keys: Vec<String> = [
        "ENTITIES/STRUCTURES/belts/belt-I.glb",
        "ENTITIES/STRUCTURES/belts/belt-L.glb",
        "ENTITIES/STRUCTURES/pipes/pipe-I.glb",
        "ENTITIES/STRUCTURES/pipes/pipe-to-ground.glb",
        "ENTITIES/STRUCTURES/heat-pipe/static.glb",
        "ENTITIES/STRUCTURES/stone-wall/static.glb",
        "ENTITIES/STRUCTURES/gate/static.glb",
        "ENTITIES/STRUCTURES/underground-belt/static.glb",
        "ENTITIES/STRUCTURES/splitter/static.glb",
    ]
    .map(String::from)
    .into();
    for tier in ["fast", "express", "turbo"] {
        keys.push(format!("ENTITIES/STRUCTURES/belts/{tier}/belt-I.glb"));
        keys.push(format!("ENTITIES/STRUCTURES/belts/{tier}/belt-L.glb"));
        keys.push(format!("ENTITIES/STRUCTURES/underground-belt/{tier}/static.glb"));
        keys.push(format!("ENTITIES/STRUCTURES/splitter/{tier}/static.glb"));
    }
    let root = REGISTRY.get().and_then(|r| r.as_ref()).map(|r| r.root.clone());
    for key in keys {
        // tier variants are optional on disk — only queue what exists
        if root.as_ref().is_none_or(|r| r.join(&key).exists()) {
            let _ = get(intern(key));
        }
    }
}

fn models_root() -> anyhow::Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = crate::dll_dir() {
        candidates.push(dir.join("models"));
        candidates.push(dir.join("..").join("..").join("models"));
    }
    candidates.push(PathBuf::from("models"));
    for c in &candidates {
        if c.is_dir() {
            return Ok(c.clone());
        }
    }
    anyhow::bail!("models/ folder not found (searched {candidates:?})")
}

// a folder's model parts. the FUE5 convention: static.glb is the housing,
// animated*.glb are the moving rigs layered on top. numbered statics are
// either pieces already contained in static.glb (chemical-plant — ignored)
// or the actual parts when there's no plain static (radar, gun-turret)
fn pick_parts(dir: &std::path::Path) -> Vec<String> {
    let mut glbs: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.to_lowercase().ends_with(".glb").then_some(n)
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    glbs.sort();
    let mut parts: Vec<String> = Vec::new();
    if glbs.iter().any(|n| n == "static.glb") {
        parts.push("static.glb".into());
    } else {
        parts.extend(glbs.iter().filter(|n| n.starts_with("static")).cloned());
    }
    parts.extend(glbs.iter().filter(|n| n.starts_with("animated")).cloned());
    if parts.is_empty() {
        parts.extend(glbs.into_iter().take(1)); // odd folder: first glb
    }
    parts
}

fn build_registry() -> anyhow::Result<Registry> {
    let root = models_root()?;
    // later categories only fill gaps: structures win over items etc.
    let categories = [
        "ENTITIES/STRUCTURES",
        "ENTITIES/VEHICLES",
        "ENTITIES/RESOURCES",
        "ENTITIES/FOLIAGE",
    ];
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    for cat in categories {
        let dir = root.join(cat);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if by_name.contains_key(&name) {
                continue;
            }
            let parts = pick_parts(&e.path());
            if !parts.is_empty() {
                by_name.insert(
                    name.clone(),
                    parts.iter().map(|g| format!("{cat}/{name}/{g}")).collect(),
                );
            }
        }
    }
    // item models (belt cargo): one static glb per item folder
    let mut items_by_name: HashMap<String, String> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(root.join("ENTITIES/ITEMS")) {
        for e in entries.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(glb) = pick_parts(&e.path()).into_iter().next() {
                items_by_name.insert(name.clone(), format!("ENTITIES/ITEMS/{name}/{glb}"));
            }
        }
    }
    if !SCAN_LOGGED.swap(true, Ordering::Relaxed) {
        log::info!(
            "[models] registry: {} model folders, {} item models under {}",
            by_name.len(),
            items_by_name.len(),
            root.display()
        );
    }

    // loader thread: gltf import is slow (big textures), never on render threads
    let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
    let thread_root = root.clone();
    std::thread::spawn(move || {
        for key in rx {
            let result = crate::gltf_model::load(&thread_root.join(key));
            let Some(Some(reg)) = REGISTRY.get() else { return };
            let mut loaded = reg.loaded.lock().unwrap();
            match result {
                Ok(m) => {
                    log::info!(
                        "[models] loaded {key}: {} nodes, {} prims, anim {:.2}s",
                        m.nodes.len(), m.prims.len(), m.duration
                    );
                    loaded.insert(key, LoadState::Ready(Arc::new(m)));
                }
                Err(e) => {
                    log::error!("[models] failed to load {key}: {e:#}");
                    loaded.insert(key, LoadState::Failed);
                }
            }
        }
    });

    Ok(Registry {
        root,
        by_name,
        items_by_name,
        resolved: Mutex::new(HashMap::new()),
        loaded: Mutex::new(HashMap::new()),
        tx,
    })
}

// item name -> interned model key, memoized. an ITEMS/<name> glb wins;
// otherwise the item's ENTITY model is used, scaled down to item size (a
// dropped machine shows as a mini version of itself). no model = vanilla
// sprite kept.
pub fn resolve_item(name: &str) -> Option<&'static str> {
    static RESOLVED: LazyLock<Mutex<HashMap<String, Option<&'static str>>>> =
        LazyLock::new(Default::default);
    let reg = REGISTRY.get()?.as_ref()?;
    memo(&RESOLVED, name.to_string(), || {
        // dedicated item model first, then fall back to the entity model
        let key = reg
            .items_by_name
            .get(name)
            .map(|rel| intern(rel.clone()))
            .or_else(|| resolve(name).map(|m| m.parts[0].key));
        if let Some(k) = key {
            log::info!("[models] item '{name}' -> {k}");
        }
        key
    })
}

// aliases: prototype name -> model folder (when names don't line up 1:1)
fn alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "coal" => "coal-ore",
        "rocket-silo-rocket" => "rocket",
        _ => return None,
    })
}

// tier variants live in subfolders next to the base glbs (belts/fast/...),
// generated by tools/make_tiers.py. missing variants fall back to the base
// model so nothing breaks when the tool hasn't run
fn tier_file(reg: &Registry, folder: &str, sub: Option<&str>, file: &str) -> String {
    if let Some(sub) = sub {
        let rel = format!("ENTITIES/STRUCTURES/{folder}/{sub}/{file}");
        if reg.root.join(&rel).exists() {
            return rel;
        }
    }
    format!("ENTITIES/STRUCTURES/{folder}/{file}")
}

// belt-family tier prefix: "fast-transport-belt" -> Some("fast")
fn belt_tier<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let p = name.strip_suffix(suffix)?.strip_suffix('-')?;
    matches!(p, "fast" | "express" | "turbo").then_some(p)
}

// inserter tier subfolder (None = the base yellow inserter)
fn inserter_tier(name: &str) -> Option<Option<&'static str>> {
    Some(match name {
        "inserter" => None,
        "burner-inserter" => Some("burner"),
        "long-handed-inserter" => Some("long-handed"),
        "fast-inserter" => Some("fast"),
        "bulk-inserter" => Some("bulk"),
        "stack-inserter" => Some("stack"),
        _ => return None, // unknown inserters resolve via their type string
    })
}

// logistic chest type subfolder (1.1 and 2.0 prototype names)
fn chest_tier(name: &str) -> Option<&'static str> {
    Some(match name {
        "logistic-chest-active-provider" | "active-provider-chest" => "active-provider",
        "logistic-chest-passive-provider" | "passive-provider-chest" => "passive-provider",
        "logistic-chest-storage" | "storage-chest" => "storage",
        "logistic-chest-buffer" | "buffer-chest" => "buffer",
        "logistic-chest-requester" | "requester-chest" => "requester",
        _ => return None,
    })
}

// resolve a prototype name to an entity model (memoized)
pub fn resolve(proto_name: &str) -> Option<EntityModel> {
    let reg = REGISTRY.get()?.as_ref()?;
    memo(&reg.resolved, proto_name.to_string(), || {
        let model = resolve_uncached(reg, proto_name);
        if let Some(m) = &model {
            log::info!(
                "[models] prototype '{proto_name}' -> {:?}",
                m.parts.iter().map(|p| p.key).collect::<Vec<_>>()
            );
        }
        model
    })
}

fn single(rel: &str, kind: ConnKind, tiles: f32) -> EntityModel {
    EntityModel {
        parts: vec![Part { key: intern(rel.to_string()), yaw: YawSource::Direction }],
        tiles,
        kind,
    }
}

// biters (the Unit class): the four size tiers map to the four glb exports
// under ENTITIES/ENEMIES/biters. returns (relative glb, footprint tiles) — the
// tiles value re-scales the shared model to each tier's size. spitters/other
// units return None and keep their vanilla sprite.
fn biter_model(name: &str) -> Option<(&'static str, f32)> {
    if !name.contains("biter") {
        return None;
    }
    // order matters: "behemoth" and "big" must be tested before the generic
    // footprints are 20% larger than the tier's real size so the biters read
    // a touch bigger on the map
    Some(if name.contains("behemoth") {
        ("ENTITIES/ENEMIES/biters/biter_tier4.glb", 3.6)
    } else if name.contains("big") {
        ("ENTITIES/ENEMIES/biters/biter_tier3.glb", 2.76)
    } else if name.contains("medium") {
        ("ENTITIES/ENEMIES/biters/biter_tier2.glb", 2.208)
    } else {
        // small-biter and any other biter variant default to the small tier
        ("ENTITIES/ENEMIES/biters/biter_tier1.glb", 1.656)
    })
}

fn resolve_uncached(reg: &Registry, proto_name: &str) -> Option<EntityModel> {
    // the player: idle key registered as the base part; the live state swaps
    // the key per frame in entities.rs. all 5 state glbs are queued now so a
    // pose switch never waits on a background load
    if proto_name == "character" {
        for key in PLAYER_KEYS {
            let _ = get(key);
        }
        return Some(EntityModel {
            parts: vec![Part { key: PLAYER_KEYS[0], yaw: YawSource::Direction }],
            tiles: 1.0, // real size comes from the PLAYER_SIZE knob at draw time
            kind: ConnKind::None,
        });
    }
    // spidertron: one rigged glb (torso + 8 IK legs), size from SPIDER_SIZE
    if proto_name == "spidertron" || proto_name.ends_with("-spidertron") {
        return Some(single("ENTITIES/VEHICLES/spidertron/static.glb", ConnKind::None, 5.0));
    }
    // biters: their own glb per size tier, facing driven by smooth orientation
    if let Some((rel, tiles)) = biter_model(proto_name) {
        return Some(EntityModel {
            parts: vec![Part { key: intern(rel.to_string()), yaw: YawSource::Orientation }],
            tiles,
            kind: ConnKind::None,
        });
    }
    // desert dead/dry trees are tree-type prototypes too (same Tree::draw
    // hook) — map them onto the ground-clutter models: bare trunks/branches
    // get the dead branch, the hairy/dry ones the dry bush
    if proto_name.starts_with("dead-") || proto_name.starts_with("dry-") {
        let rel = if proto_name.contains("trunk") || proto_name.contains("dead-tree") {
            "BLUEPRINTS/ground-system/ground-clutter/assets/branch1/static.glb"
        } else {
            "BLUEPRINTS/ground-system/ground-clutter/assets/bush1/static.glb"
        };
        return Some(single(rel, ConnKind::None, 1.4));
    }
    // rocks/stones are SimpleEntity prototypes (big-rock, huge-rock,
    // big-sand-rock, rock-big, sand-rock-big, ...) — all map onto the one
    // ground-clutter rock model, sized to the footprint. guard against the
    // rocket-silo family, whose names also contain "rock"
    if proto_name.contains("rock") && !proto_name.contains("rocket") {
        let rel = "BLUEPRINTS/ground-system/ground-clutter/assets/rock1/static.glb";
        let tiles = if proto_name.contains("huge") { 2.6 } else { 2.0 };
        return Some(single(rel, ConnKind::None, tiles));
    }
    // trees: map every tree prototype (tree-01, tree-02, ...) onto one of the
    // 5 FOLIAGE models, spread by a hash of the name so a mixed forest varies
    if proto_name.starts_with("tree") {
        let h = proto_name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        let n = 1 + (h % 5);
        return Some(single(&format!("ENTITIES/FOLIAGE/tree{n}/static.glb"), ConnKind::None, 1.5));
    }
    // connectable families: shared models, tier prefixes pick a recolored copy
    if proto_name == "transport-belt" || proto_name.ends_with("-transport-belt") {
        let rel = tier_file(reg, "belts", belt_tier(proto_name, "transport-belt"), "belt-I.glb");
        return Some(single(&rel, ConnKind::Belt, 1.0));
    }
    if proto_name == "underground-belt" || proto_name.ends_with("-underground-belt") {
        let sub = belt_tier(proto_name, "underground-belt");
        return Some(single(&tier_file(reg, "underground-belt", sub, "static.glb"), ConnKind::Under, 1.0));
    }
    if proto_name == "splitter" || proto_name.ends_with("-splitter") {
        let sub = belt_tier(proto_name, "splitter");
        return Some(single(&tier_file(reg, "splitter", sub, "static.glb"), ConnKind::Splitter, 2.0));
    }
    // inserter tiers: recolored (long-handed: also arm-stretched) variants
    if let Some(sub) = inserter_tier(proto_name) {
        return Some(EntityModel {
            parts: ["static.glb", "animated1.glb"]
                .iter()
                .map(|f| Part {
                    key: intern(tier_file(reg, "inserter", sub, f)),
                    yaw: YawSource::Direction,
                })
                .collect(),
            tiles: 1.0,
            kind: ConnKind::None,
        });
    }
    // logistic chests: one grey base model, colorized per chest type
    if let Some(sub) = chest_tier(proto_name) {
        let rel = tier_file(reg, "logistic-chests", Some(sub), "static.glb");
        return Some(single(&rel, ConnKind::None, 1.0));
    }
    // assembler tiers 2/3: recolored copies with the same part layout
    if let Some(tier) = proto_name.strip_prefix("assembling-machine-") {
        let dir = reg.root.join("ENTITIES/STRUCTURES/assembling-machine").join(tier);
        let parts = pick_parts(&dir);
        if !parts.is_empty() {
            return Some(EntityModel {
                parts: parts
                    .iter()
                    .map(|g| Part {
                        key: intern(format!("ENTITIES/STRUCTURES/assembling-machine/{tier}/{g}")),
                        yaw: YawSource::Direction,
                    })
                    .collect(),
                tiles: footprint(proto_name),
                kind: ConnKind::None,
            });
        }
    }
    if proto_name == "pipe" {
        return Some(single("ENTITIES/STRUCTURES/pipes/pipe-I.glb", ConnKind::Pipe, 1.0));
    }
    if proto_name == "pipe-to-ground" {
        return Some(single(
            "ENTITIES/STRUCTURES/pipes/pipe-to-ground.glb",
            ConnKind::PipeEnd,
            1.0,
        ));
    }
    if proto_name == "heat-pipe" {
        return Some(single("ENTITIES/STRUCTURES/heat-pipe/static.glb", ConnKind::Heat, 1.0));
    }

    // radar: fixed platform (static1) + continuously spinning dish (static2)
    if proto_name == "radar" {
        let dir = "ENTITIES/STRUCTURES/radar";
        if reg.root.join(dir).join("static1.glb").exists()
            && reg.root.join(dir).join("static2.glb").exists()
        {
            return Some(EntityModel {
                parts: vec![
                    Part { key: intern(format!("{dir}/static1.glb")), yaw: YawSource::Direction },
                    Part { key: intern(format!("{dir}/static2.glb")), yaw: YawSource::Spin },
                ],
                tiles: footprint(proto_name),
                kind: ConnKind::None,
            });
        }
    }

    // gun turret: fixed base (static1) + orientation-rotated head (static2)
    if proto_name == "gun-turret" {
        let dir = "ENTITIES/STRUCTURES/gun-turret";
        if reg.root.join(dir).join("static1.glb").exists()
            && reg.root.join(dir).join("static2.glb").exists()
        {
            return Some(EntityModel {
                parts: vec![
                    Part { key: intern(format!("{dir}/static1.glb")), yaw: YawSource::Fixed },
                    Part { key: intern(format!("{dir}/static2.glb")), yaw: YawSource::Orientation },
                ],
                tiles: footprint(proto_name),
                kind: ConnKind::None,
            });
        }
    }

    let mut candidates: Vec<String> = vec![proto_name.to_string()];
    if let Some(a) = alias(proto_name) {
        candidates.push(a.to_string());
    }
    // strip a trailing tier number: assembling-machine-3 -> assembling-machine
    if let Some(idx) = proto_name.rfind('-') {
        if proto_name[idx + 1..].chars().all(|c| c.is_ascii_digit()) {
            candidates.push(proto_name[..idx].to_string());
        }
    }
    let kind = if proto_name.ends_with("-wall") {
        ConnKind::Wall
    } else if proto_name == "gate" {
        ConnKind::Gate
    } else {
        ConnKind::None
    };
    for c in candidates {
        if let Some(rels) = reg.by_name.get(&c) {
            return Some(EntityModel {
                parts: rels
                    .iter()
                    .map(|rel| Part { key: intern(rel.clone()), yaw: YawSource::Direction })
                    .collect(),
                tiles: footprint(proto_name),
                kind,
            });
        }
    }
    None
}

// request a model; queues a background load on first ask.
// returns Some(model) once ready, None while loading / after failure
pub fn get(key: &'static str) -> Option<Arc<ModelData>> {
    let reg = REGISTRY.get()?.as_ref()?;
    let mut loaded = reg.loaded.lock().unwrap();
    match loaded.get(key) {
        Some(LoadState::Ready(m)) => Some(m.clone()),
        Some(_) => None,
        None => {
            loaded.insert(key, LoadState::Loading);
            let _ = reg.tx.send(key);
            None
        }
    }
}

// once a model is Ready it stays ready — cache (ptr -> clip duration) behind
// a read-mostly RwLock so the per-entity-per-frame ready/duration checks
// (record's all_ready from every prepare thread, advance_anim from tick)
// stop serializing on the loader's Mutex
static READY_META: LazyLock<std::sync::RwLock<crate::util::FxHashMap<usize, f32>>> =
    LazyLock::new(Default::default);

// Some(clip duration) once the model is loaded; triggers the background
// load on first ask, like get()
pub fn ready_duration(key: &'static str) -> Option<f32> {
    let ptr = key.as_ptr() as usize;
    if let Some(d) = READY_META.read().unwrap().get(&ptr) {
        return Some(*d);
    }
    let m = get(key)?;
    let d = m.duration;
    READY_META.write().unwrap().insert(ptr, d);
    Some(d)
}

// true when every part of the model is loaded (entity sprite can be hidden)
pub fn all_ready(model: &EntityModel) -> bool {
    model.parts.iter().all(|p| ready_duration(p.key).is_some())
}

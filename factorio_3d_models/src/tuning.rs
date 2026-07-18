// live-tunable knobs: f3dm_tuning.txt next to the dll, re-read every
// second while the game runs. lets model orientation get calibrated by eye
// without a rebuild + game restart per guess.

use crate::util::{AtomicF32, FxHashMap};
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! knobs {
    ($($name:ident = $default:expr;)*) => {
        $( pub static $name: AtomicF32 = AtomicF32::new($default); )*

        static KNOBS: &[(&str, &AtomicF32, f32)] =
            &[ $( (stringify!($name), &$name, $default), )* ];
    };
}

// yaws in degrees, *_FLIP / *_MIRROR are 0 or 1
knobs! {
    BELT_I_YAW = 0.0;      // straight belt facing north
    BELT_L_YAW = 0.0;      // corner belt
    BELT_L_MIRROR = 0.0;   // 1 = swap left/right corners
    UG_YAW = -90.0;        // underground belt (model lies along x)
    SPLITTER_YAW = 0.0;
    SPLITTER_UV_FLIP = 1.0; // 1 = reverse the splitter tread scroll direction
    PIPE_YAW = 0.0;        // extra on straight pipes / heat pipes
    PTG_YAW = 0.0;         // pipe-to-ground
    WALL_YAW = 0.0;
    GATE_YAW = 0.0;        // gate facing offset (dir 0 = north-south passage)
    PUMPJACK_MIRROR = 1.0; // 1 = mirror-flip the pumpjack (model ships reflected)
    VEHICLE_YAW = 90.0;    // trains/cars (models lie along x)
    BITER_YAW = 180.0;     // biter facing offset (model authored facing -Y = backwards)
    HEAD_YAW = 90.0;       // gun turret head
    ORIENT_FLIP = 0.0;     // 1 = reverse the smooth-orientation spin
    CORNER_YAW = 0.0;      // pipe corner elbow (reused pipe-to-ground)
    CORNER_ROLL = 90.0;    // roll that lays the down-bend sideways
    PIPE_LIFT = 0.55;      // raise straight pipes (tiles)
    HEAT_LIFT = 0.0;       // heat pipes hug the ground
    CORNER_LIFT = 0.55;    // raise the corner elbows
    CORNER_DX = 0.5;       // center the corner elbow in its tile (tiles,
    CORNER_DY = 0.3;       // for the N+E corner — other corners rotate along)
    BELT_SPEED = -1.875;   // tread uv scroll, texture repeats/second
    BELT_UV_U = 1.0;       // 1 = scroll u (along the belt), 0 = v
    ITEM_SIZE = 0.45;      // 3d item footprint on belts, tiles
    ITEM_LIFT = 0.3;       // items ride this high above ground, tiles
    ROBOT_LIFT = 2.5;      // flying robots hover this high above ground, tiles
    ANIM_SPEED = 1.0;      // global glb animation speed multiplier (inserters,
                           // machines...) — raise if arms swing too slowly
    INSERTER_REACH = 0.85; // radius the held item swings out from the inserter
    INSERTER_ITEM_LIFT = 0.6; // held item hovers this high (in the hand), tiles
    INSERTER_ITEM_SIZE = 0.4; // held item footprint, tiles (0 = fall back to ITEM_SIZE)
    // the inserter arm clip is a symmetric out-and-back; map the real arm's
    // absolute deviation from its facing (0..this turns) onto it. a normal
    // inserter sweeps ~0.5 turn from drop (front) to pickup (behind), so 0.5
    // reaches full model extension at full swing. LOWER if the model arm reaches
    // full extension too early (looks fast/clipped), RAISE if it never fully
    // extends. driven by absolute position, so there is NO start lag. 0 =
    // disable (fall back to the free-running working-window animation)
    INSERTER_ARM_RANGE = 0.5;
    // the model arm clip only swings one way; the real arm swings to whichever
    // side of its facing the pickup is on. we mirror the model when the live arm
    // is on the negative side of its facing so the swing matches for every
    // direction. flip this (0<->1) if the mirrored directions are the wrong pair
    INSERTER_SWING_FLIP = 0.0;
    ROCKET_RISE_SCALE = 1.0; // multiplies the silo rocket's flight height
    ROCKET_BASE_LIFT = -3.0; // sink a loaded rocket so only its head sticks out
                             // (below-ground geometry is clipped away)
    ROCKET_EMERGE_SECS = 3.0; // rise-from-silo animation length when built
    ROCKET_EMERGE_DROP = 3.0; // how far below resting it starts (tiles)
    TANK_YAW = 0.0;           // tank facing (model lies along z, unlike trains)
    CAR_YAW = 0.0;            // car facing (exported in the tank convention)
    TANK_TURRET_FLIP = 0.0;   // 1 = reverse the tank turret spin
    TANK_TRACK_SPEED = 1.0;   // track scroll vs ground speed (sign = direction)
    TANK_WHEEL_FLIP = 1.0;    // 1 = reverse the road wheel spin
    TANK_TURN_GAUGE = 0.9;    // chain tiles per radian when turning in place
    // the game reports wire height in PIXELS (32/tile): 105.5 on big poles
    WIRE_HEIGHT_SCALE = 0.03125; // multiplies the game's wire render height
    WIRE_HEIGHT_ADD = 0.0;    // extra endpoint height, tiles (all wires)
    WIRE_POLE_ADD = 0.4;      // extra height at electric-pole ends (not subs)
    // the game pre-shifts wire endpoints up-screen (north) by the render
    // height — shift them back south by this fraction of the height
    WIRE_SHIFT_Y = 1.0;
    WIRE_SAG = 0.3;           // catenary dip at the middle, tiles
    WIRE_WIDTH = 0.05;        // wire thickness, tiles
    // drop a captured wire span if it hasn't re-recorded within this many
    // frames — clears the fan of orphan preview spans left while dragging a
    // new wire. lower = clears faster; raise if real static wires flicker/vanish
    WIRE_EXPIRE_FRAMES = 20.0;
    // hold on-belt items this many frames after they stop recording, to bridge
    // cached-belt redraws without flicker. lower = items clear faster when a
    // belt is removed
    ITEM_CLEAR_FRAMES = 30.0;
    // one chain link: 0.354 model units (array const offset in Tank_Baked.blend)
    // / 8.37 units extent * 4 tiles footprint. only used WITHOUT a
    // track_path.json (the conveyor path wraps at the real loop length)
    TANK_TRACK_PERIOD = 0.169; // tread repeat length in tiles (scroll wraps here)

    // --- the player character ---------------------------------------------
    PLAYER_YAW = 180.0;    // mixamo rig facing offset (same authored facing as biters)
    PLAYER_SIZE = 0.6;     // model xz extent fitted to this many tiles
    PLAYER_TINT = 0.75;    // 0 = ignore the /color player color, 1 = full tint
    PLAYER_BELT_LIFT = 0.4; // stand this high on belts (belt-top height, tiles)
    PLAYER_SMOOTH = 0.35;  // render-position smoothing (the raw field steps at
                           // 60UPS and judders against the camera). 1 = raw
    PLAYER_RUN_REF = 2.4;  // ground speed (tiles/s) that plays the run clip at 1x
    PLAYER_ANIM_SPEED = 1.0; // global player clip speed multiplier
    PLAYER_SHOOT_WINDOW = 40.0; // frames a shot keeps the shooting pose (covers
                                // slow gun cooldowns between shots)
    PLAYER_MINE_WINDOW = 12.0;  // frames a mining tick keeps the mining pose
    // hide the model when the character stops re-recording for this many
    // frames (entered a vehicle / remote view). raise if the idle player
    // blinks or vanishes; lower if a ghost lingers when entering vehicles
    PLAYER_EXPIRE_FRAMES = 30.0;
    // first person: hide the local player's model above this fraction of its
    // height (the head blocks the camera). the body below stays; other
    // players are untouched. raise if the shoulders vanish, lower if hair
    // still pokes into view. 0 = keep the head
    PLAYER_HEAD_CLIP = 0.78;

    // --- model shading (the PBR-ish look) ---------------------------------
    // -1 = auto: follow the game's live darkness (day/night cycle). 0..1 =
    // manual override (0 full day, 1 full night). dims the sun + ambient,
    // tints toward moonlight, and boosts emissive so lamps light up
    NIGHT = -1.0;
    // auto mode: scales game darkness -> night. vanilla darkness tops out
    // around 0.85, so >1 lets midnight reach the full moonlit look
    NIGHT_GAIN = 1.15;
    LIGHT_SUN = 1.0;      // directional sun strength (specular + key light)
    LIGHT_AMBIENT = 0.6;  // hemispheric sky/ground fill strength
    LIGHT_REFLECT = 0.6;  // fake environment reflection (metal mirrors sky/ground)
    LIGHT_RIM = 0.22;     // fresnel edge light toward the sky (UE-ish pop)
    LIGHT_EXPOSURE = 0.85; // pre-tonemap exposure (brightness of the whole look)
    LIGHT_EMISSIVE = 1.0; // global multiplier on authored emissive glow
    LIGHT_SPEC = 1.0;     // specular highlight strength multiplier
    LIGHT_ROUGH = 1.0;    // multiplies material roughness (lower = sharper shine)
    // emissive flicker at night (furnace fire / lamp shimmer, mimics the FUE5
    // light-function materials). 0 = steady glow
    LIGHT_FLICKER = 0.12;
    // ambient floor kept at night so shadowed sides don't go pure black
    NIGHT_AMBIENT = 0.28;
    RADAR_SPIN = 60.0;   // radar dish spin speed, degrees/second (while powered)
    GHOST_ALPHA = 0.35;  // ghost-entity 3d preview opacity (0 = off)
    // skip the ground-shadow pass when the view spans more tiles than this:
    // at far zoom shadows are sub-pixel but still cost a full extra
    // geometry pass over every instance (0 = never skip)
    SHADOW_MAX_SPAN = 100.0;
    // lamps light nearby models at night (point lights in the shader)
    LAMP_RADIUS = 6.0;   // falloff radius, tiles
    LAMP_STRENGTH = 2.2; // brightness of the warm lamp pool
}

// per-model yaw overrides: lines like "yaw:oil-refinery=180" — matched by
// substring against the model key, additive on top of everything else
static MODEL_YAWS: Mutex<Vec<(String, f32)>> = Mutex::new(Vec::new());
// per-model placement offsets: lines like "off:oil-refinery=0.5,-0.5,0.2"
// (dx east, dy south, dz up — tiles), substring-matched, additive
static MODEL_OFFSETS: Mutex<Vec<(String, [f32; 3])>> = Mutex::new(Vec::new());
// extra part placements: "copy:centrifuge/animated1=dx,dy,dz" (or =dx,dy,dz,
// yawdeg) draws the matched model AGAIN at an entity-relative offset (tiles,
// rotated with the entity facing). the FUE5 centrifuge ships ONE animated
// column authored at one of its three sockets — two copy lines fill the rest
static MODEL_COPIES: Mutex<Vec<(String, [f32; 4])>> = Mutex::new(Vec::new());

// the three lookups below run per instance per frame — memoized by the
// interned key's pointer, invalidated (generation bump) when the tuning
// file actually changes the lists
static TWEAK_GEN: AtomicU64 = AtomicU64::new(0);

fn tweak_memo<T: Clone>(
    cache: &Mutex<(u64, FxHashMap<usize, T>)>,
    key: &'static str,
    compute: impl FnOnce() -> T,
) -> T {
    let generation = TWEAK_GEN.load(Ordering::Relaxed);
    let mut c = cache.lock().unwrap();
    if c.0 != generation {
        c.0 = generation;
        c.1.clear();
    }
    c.1.entry(key.as_ptr() as usize).or_insert_with(compute).clone()
}

pub fn model_yaw_extra(key: &'static str) -> f32 {
    static CACHE: LazyLock<Mutex<(u64, FxHashMap<usize, f32>)>> =
        LazyLock::new(Default::default);
    tweak_memo(&CACHE, key, || {
        MODEL_YAWS
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| key.contains(name.as_str()))
            .map(|(_, deg)| deg.to_radians())
            .sum()
    })
}

// every "copy:<name>=dx,dy,dz[,yaw]" whose name is in the key — each entry
// is one EXTRA placement of the part (the original always draws)
pub fn model_copies(key: &'static str) -> std::sync::Arc<Vec<[f32; 4]>> {
    static CACHE: LazyLock<Mutex<(u64, FxHashMap<usize, std::sync::Arc<Vec<[f32; 4]>>>)>> =
        LazyLock::new(Default::default);
    tweak_memo(&CACHE, key, || {
        std::sync::Arc::new(
            MODEL_COPIES
                .lock()
                .unwrap()
                .iter()
                .filter(|(name, _)| key.contains(name.as_str()))
                .map(|(_, v)| *v)
                .collect(),
        )
    })
}

// summed dx/dy/dz (tiles) for every "off:<name>=..." whose name is in the key
pub fn model_offset_extra(key: &'static str) -> [f32; 3] {
    static CACHE: LazyLock<Mutex<(u64, FxHashMap<usize, [f32; 3]>)>> =
        LazyLock::new(Default::default);
    tweak_memo(&CACHE, key, || {
        let list = MODEL_OFFSETS.lock().unwrap();
        let mut o = [0.0f32; 3];
        for (_, d) in list.iter().filter(|(name, _)| key.contains(name.as_str())) {
            o[0] += d[0];
            o[1] += d[1];
            o[2] += d[2];
        }
        o
    })
}

static LAST_POLL_FRAME: AtomicU64 = AtomicU64::new(0);
static WROTE_TEMPLATE: Mutex<bool> = Mutex::new(false);

fn file_path() -> Option<std::path::PathBuf> {
    Some(crate::dll_dir()?.join("f3dm_tuning.txt"))
}

pub fn init() {
    poll(u64::MAX); // load the file once right away
}

// called once per frame; re-reads the file every ~60 frames
pub fn poll(frame: u64) {
    let last = LAST_POLL_FRAME.load(Ordering::Relaxed);
    if frame != u64::MAX && frame.saturating_sub(last) < 60 {
        return;
    }
    // the one-shot init call passes u64::MAX — store 0, not MAX, or every later
    // frame computes frame - MAX = 0 < 60 and NEVER re-reads (live tuning dead)
    LAST_POLL_FRAME.store(if frame == u64::MAX { 0 } else { frame }, Ordering::Relaxed);
    let Some(path) = file_path() else { return };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let mut model_yaws: Vec<(String, f32)> = Vec::new();
            let mut model_offsets: Vec<(String, [f32; 3])> = Vec::new();
            let mut model_copies: Vec<(String, [f32; 4])> = Vec::new();
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((k, v)) = line.split_once('=') else { continue };
                let (k, v) = (k.trim(), v.trim());
                // "off:<model-name>=dx,dy,dz" -> per-model placement offset (tiles)
                if let Some(name) = k.strip_prefix("off:") {
                    let p: Vec<f32> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                    if p.len() == 3 {
                        model_offsets.push((name.trim().to_string(), [p[0], p[1], p[2]]));
                    } else {
                        log::warn!("[tuning] bad line ignored (want dx,dy,dz): {line}");
                    }
                    continue;
                }
                // "copy:<model-name>=dx,dy,dz" or "=dx,dy,dz,yawdeg"
                // -> extra part placement
                if let Some(name) = k.strip_prefix("copy:") {
                    let p: Vec<f32> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                    if p.len() == 3 || p.len() == 4 {
                        model_copies.push((
                            name.trim().to_string(),
                            [p[0], p[1], p[2], p.get(3).copied().unwrap_or(0.0)],
                        ));
                    } else {
                        log::warn!(
                            "[tuning] bad line ignored (want dx,dy,dz or dx,dy,dz,yaw): {line}"
                        );
                    }
                    continue;
                }
                let Ok(val) = v.parse::<f32>() else { continue };
                // "yaw:<model-name>=deg" -> per-model override
                if let Some(name) = k.strip_prefix("yaw:") {
                    model_yaws.push((name.trim().to_string(), val));
                    continue;
                }
                let k = k.to_uppercase();
                for (name, knob, _) in KNOBS {
                    if *name == k {
                        knob.set(val);
                    }
                }
            }
            // only bump the memo generation when the lists actually changed —
            // poll rewrites them every second even for an untouched file
            let changed = {
                *MODEL_YAWS.lock().unwrap() != model_yaws
                    || *MODEL_OFFSETS.lock().unwrap() != model_offsets
                    || *MODEL_COPIES.lock().unwrap() != model_copies
            };
            *MODEL_YAWS.lock().unwrap() = model_yaws;
            *MODEL_OFFSETS.lock().unwrap() = model_offsets;
            *MODEL_COPIES.lock().unwrap() = model_copies;
            if changed {
                TWEAK_GEN.fetch_add(1, Ordering::Relaxed);
            }
            // confirm the file is actually being read + which values are live,
            // so "my edits do nothing" can be diagnosed from the log
            log::info!(
                "[tuning] read {}: corner_dx={} corner_dy={} wire_pole_add={}",
                path.display(),
                CORNER_DX.get(),
                CORNER_DY.get(),
                WIRE_POLE_ADD.get()
            );
        }
        Err(_) => {
            // write a template so the user can just edit it
            let mut wrote = WROTE_TEMPLATE.lock().unwrap();
            if !*wrote {
                *wrote = true;
                let mut text = String::from(
                    "# live yaw tuning (degrees), re-read every second\n\
                     # per-model override: yaw:oil-refinery=180 (substring match)\n\
                     # per-model offset: off:oil-refinery=dx,dy,dz (tiles)\n\
                     # extra part placement: copy:centrifuge/animated1=dx,dy,dz[,yawdeg]\n",
                );
                for (name, _, def) in KNOBS {
                    text.push_str(&format!("{}={}\n", name.to_lowercase(), def));
                }
                if std::fs::write(&path, text).is_ok() {
                    log::info!("[tuning] template written: {}", path.display());
                }
            }
        }
    }
}

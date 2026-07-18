// all tunable numbers in one place

// master switch for rendering extra world while tilted (the "horizon boost")
pub const HORIZON_BOOST: bool = true;

// target world span (tiles, wider axis) when tilted, independent of zoom.
// without the hi-res capture machinery of the big mod, larger spans get
// blurrier — kept moderate here
pub const TARGET_SPAN_TILES: f64 = 140.0;

// cap on the zoom-out boost factor
pub const MAX_ZOOM_BOOST: f64 = 2.5;

// hard cap on the boosted world span (tiles)
pub const MAX_BOOST_SPAN_TILES: f64 = 300.0;

// below this render scale the game switches to simplified zoomed-out sprites
pub const MIN_EFFECTIVE_SCALE: f64 = 0.25;

// shifts the orbit pivot toward the lower foreground when tilted, so the
// camera visibly circles the player instead of the far horizon
pub const LOOK_AHEAD: f32 = 0.5;

// background color where the tilted plane doesn't reach (plain dark sky)
pub const SKY_COLOR: [f32; 3] = [0.05, 0.07, 0.12];

// each model's xz extent is auto-fitted to its entity's tile footprint,
// times this multiplier (1.0 = model exactly covers the footprint; vanilla
// sprites overhang slightly, so >1 looks closer to the original game)
pub const MODEL_SCALE: f32 = 1.0;

// extra yaw applied to every model (degrees); per-family offsets live in
// f3dm_tuning.txt next to the dll (hot-reloaded every second)
pub const MODEL_YAW_DEG: f32 = 0.0;

// sun direction the light TRAVELS (matches vanilla sprites: sun in the west,
// shadows falling east and slightly south). x = east, y = up, z = north
pub const SUN_DIR: [f32; 3] = [1.0, -1.2, -0.35];

// ground shadow opacity (vanilla sprite shadows are ~half dark)
pub const SHADOW_ALPHA: f32 = 0.45;

// --- model lighting palette (linear-ish rgb, the shader gamma-corrects) -----
// the FUE5 look isn't in the glb — it's Unreal's PBR + sky lighting. we fake
// that: a warm directional sun plus a hemispheric ambient (cool sky from above,
// darker warm bounce from below). strengths/exposure/night live in f3dm_tuning.

// warm midday sun color the directional light is tinted with
pub const SUN_COLOR: [f32; 3] = [1.0, 0.95, 0.85];
// golden-hour sun the light bends toward around dawn/dusk (mid darkness)
pub const DUSK_SUN_COLOR: [f32; 3] = [1.0, 0.55, 0.28];
// sky ambient (fills shadows with a soft cool light from above)
pub const SKY_AMBIENT: [f32; 3] = [0.42, 0.48, 0.60];
// ground bounce (darker, warmer light on downward faces)
pub const GROUND_AMBIENT: [f32; 3] = [0.18, 0.15, 0.12];
// night sky tint the whole scene is pushed toward as NIGHT -> 1 (moonlight)
pub const NIGHT_COLOR: [f32; 3] = [0.16, 0.20, 0.32];

// a machine counts as working while its animation-state fingerprint changed
// within this many frames (working machines re-draw every frame)
pub const WORKING_WINDOW_FRAMES: u64 = 8;

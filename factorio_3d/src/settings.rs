// all tunable numbers in one place

// master switch for rendering extra world while tilted (the "horizon boost")
pub const HORIZON_BOOST: bool = true;

// target world span (tiles, wider axis) when tilted, independent of zoom.
// raised for a bigger view — the far field gets blurrier (game LOD wall)
pub const TARGET_SPAN_TILES: f64 = 300.0;

// cap on the zoom-out boost factor. higher = deeper horizon but blurrier
pub const MAX_ZOOM_BOOST: f64 = 5.0;

// hard cap on the boosted world span (tiles)
pub const MAX_BOOST_SPAN_TILES: f64 = 550.0;

// below this render scale the game switches to simplified zoomed-out sprites,
// which look bad as billboards — the boost never goes past it
pub const MIN_EFFECTIVE_SCALE: f64 = 0.25;

// how far belts float above the ground (tiles)
pub const BELT_LIFT_TILES: f32 = 0.5;

// height of the elevated-rail deck (tiles). must match the height the rail
// RAMP art rises to (~3 tiles), or the deck disconnects from the ramp tops —
// a flat plane can't slope, so this is the join point. south shift is 0 for
// the same reason (shifting the deck slides it off the ramps).
pub const ELEVATED_LIFT_TILES: f32 = 3.0;
pub const ELEVATED_SOUTH_TILES: f32 = 0.0;

// solar panels + rocket silo: laid flat, raised onto a low platform this high
pub const FLAT_PLATFORM_TILES: f32 = 0.6;

// agricultural tower crane arm: nudged up and south
pub const CRANE_LIFT_TILES: f32 = 1.5;
pub const CRANE_SOUTH_TILES: f32 = 1.0;

// sky (the background the tilted canvas doesn't cover). per planet: day rgb +
// night rgb; the current darkness (0 day .. 1 night) blends day -> night.
// planet ids: 0 nauvis, 1 vulcanus, 2 fulgora, 3 gleba, 4 aquilo (else nauvis)
pub fn sky_day_night(planet: u8) -> ([f32; 3], [f32; 3]) {
    match planet {
        1 => ([0.16, 0.09, 0.05], [0.05, 0.03, 0.02]), // vulcanus dark brown
        2 => ([0.14, 0.06, 0.18], [0.05, 0.02, 0.08]), // fulgora dark purple
        3 => ([0.10, 0.20, 0.16], [0.03, 0.07, 0.06]), // gleba blue-greenish
        4 => ([0.06, 0.10, 0.18], [0.02, 0.04, 0.08]), // aquilo dark blue
        _ => ([0.32, 0.52, 0.85], [0.02, 0.03, 0.07]), // nauvis blue day / dark night
    }
}

// height of the wire plane (tiles, roughly pole-connection height)
pub const WIRE_LIFT_TILES: f32 = 3.8;

// flying robots (logistic/construction bots): billboards lifted into the air
// and shifted south (their sprite sits above the shadow, like the wire trick)
pub const BOT_LIFT_TILES: f32 = 2.5;
pub const BOT_SOUTH_TILES: f32 = 1.0;

// direction sprites rotate to face the camera. +1.0 is correct: a standing
// entity keeps facing the same world direction, so its frame steps the same
// way the ground turns on screen
pub const CHAR_ROT_SIGN: f64 = 1.0;

// cap on sprite rects recorded per frame (a full re-queue of a big base
// can produce a lot at once)
pub const MAX_RECTS: usize = 131072;

// cap on billboard quads drawn per frame
pub const MAX_BILLBOARDS: usize = 65536;

// how tall standing billboards are relative to the flat area they covered
pub const STAND_SCALE: f32 = 1.25;

// shifts the orbit pivot toward the lower foreground when tilted, so the
// camera visibly circles the player instead of the far horizon
pub const LOOK_AHEAD: f32 = 0.5;

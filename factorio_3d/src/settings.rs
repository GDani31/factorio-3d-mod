// all tunable numbers in one place

// master switch for rendering extra world while tilted (the "horizon boost")
pub const HORIZON_BOOST: bool = true;

// target world span (tiles, wider axis) when tilted, independent of zoom
pub const TARGET_SPAN_TILES: f64 = 220.0;

// cap on the zoom-out boost factor. higher = deeper horizon but blurrier
pub const MAX_ZOOM_BOOST: f64 = 4.0;

// hard cap on the boosted world span (tiles)
pub const MAX_BOOST_SPAN_TILES: f64 = 400.0;

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

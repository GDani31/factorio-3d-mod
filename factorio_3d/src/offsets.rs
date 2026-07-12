// everything specific to one factorio build (2.0.77) lives here.
//
// each hooked game function has a pdb symbol-name fragment and a fallback
// rva (offset from the exe base). functions are found by NAME first, so most
// hooks survive game updates; the rvas are only used when the symbol is
// missing. after a game update, re-check the rvas of the entries with an
// empty symbol (file-local functions with no public symbol).

// one hookable game function: pdb name fragment + fallback address
pub struct GameFn {
    // substring of the mangled symbol name ("" = no symbol, rva only)
    pub symbol: &'static str,
    // fallback offset from the exe base (factorio 2.0.77)
    pub rva: usize,
}

const fn f(symbol: &'static str, rva: usize) -> GameFn {
    GameFn { symbol, rva }
}

// --- frame / render pipeline -------------------------------------------------
pub const ENTITY_RENDERER_PREPARE: GameFn = f("?prepare@EntityRenderer", 0x00656AD0);
pub const GAME_RENDERER_RENDER: GameFn = f("?render@GameRenderer@@", 0x0065B3C0);
pub const GAME_RENDERER_ACK_RESIZE: GameFn = f("?acknowledgeResize@GameRenderer@@", 0x0065B830);
pub const CREATE_RENDER_PARAMS: GameFn = f("?createRenderParameters@GameView@@", 0x0010B7F0);
// centerOn writes the real scale (displayScale x zoom); the horizon boost
// divides the zoom around this call
pub const RP_CENTER_ON: GameFn = f("?centerOn@RenderParameters@@", 0x00682A20);
// per-frame framebuffer-size chokepoint; also signals level loads
pub const GCEF_SET_SIZE: GameFn = f("?setSize@GarbageCollectableEngineFramebuffers@@", 0x007AE830);
// layer-range draw dispatcher — object/belt ranges get diverted into captures
pub const DRAW_ENTITIES: GameFn = f("?drawEntities@DrawEngine@@", 0x0063F700);
// sprite-batch flush; force-flushed at capture window boundaries
pub const DCB_FLUSH: GameFn = f("?flush@DrawCommandBatch@@", 0x00639740);

// --- sprite queue methods (every world sprite goes through one of these) -----
pub const DQ_DRAW_VEC: GameFn = f(
    "?draw@DrawQueue@@QEAAXAEBVSprite@@AEBVMapPosition@@VDrawingFlags@@W4Enum@RenderLayer@@AEBVVector",
    0x006442C0,
);
pub const DQ_DRAW_PAIR: GameFn = f(
    "?draw@DrawQueue@@QEAAXAEBVSprite@@AEBVMapPosition@@VDrawingFlags@@W4Enum@RenderLayer@@AEBU",
    0x00644630,
);
pub const DQ_DRAW_TINTED: GameFn = f("?drawTinted@DrawQueue@@", 0x006469C0);
pub const DQ_DRAW_ROTATED: GameFn = f("?drawRotated@DrawQueue@@", 0x00647400);
pub const DQ_DRAW_ROTATED_TINTED: GameFn = f("?drawRotatedTinted@DrawQueue@@", 0x0064B430);
pub const DQ_DRAW_ROTATED_WITHOUT_TINT: GameFn =
    f("?drawRotatedWithoutTint@DrawQueue@@", 0x006470D0);
pub const DQ_DRAW_SHIFTED_ROT: GameFn = f(
    "?drawShiftedBeforeRotatedWithoutTint@DrawQueue@@QEAAXAEBVSprite",
    0x00649EB0,
);
// the train path (carries a precomputed sin/cos pair)
pub const DQ_DRAW_SHIFTED_ROT_PRECISE: GameFn =
    f("?drawShiftedBeforeRotatedWithoutTintPrecise@DrawQueue@@", 0x0064A380);
// the inserter-arm path
pub const DQ_DRAW_SCALED_ROTATED: GameFn = f("?drawScaledRotated@DrawQueue@@", 0x0064B0B0);
// file-local sprite placement helper (no symbol) — computes each sprite's
// on-screen rect; hooked to record billboard rects
pub const PLACE_SPRITE: GameFn = f("", 0x0069C310);

// --- player / input -----------------------------------------------------------
pub const COMPUTE_WALK_DIR: GameFn =
    f("?computeWalkingDirectionFromPlayerInputs@PlayerInputSource@@", 0x00A95B90);
pub const GET_MAP_POSITION: GameFn = f(
    "?getMapPosition@GameView@@QEBA?AVMapPosition@@VPixelPosition@@@Z",
    0x0010E760,
);

// --- entity draw methods (bracketed to record their sprites) -----------------
pub const CHARACTER_DRAW: GameFn = f("?draw@Character@@UEBAXAEAVDrawQueue@@@Z", 0x00373BA0);
pub const CAR_DRAW: GameFn = f("?draw@Car@@UEBAXAEAVDrawQueue@@@Z", 0x00351640);
pub const LOCOMOTIVE_DRAW: GameFn = f("?draw@Locomotive@@UEBAXAEAVDrawQueue@@@Z", 0x00461A30);
pub const CARGO_WAGON_DRAW: GameFn = f("?draw@CargoWagon@@UEBAXAEAVDrawQueue@@@Z", 0x003695D0);
pub const ROLLING_STOCK_DRAW: GameFn = f("?draw@RollingStock@@UEBAXAEAVDrawQueue@@@Z", 0x004F3AB0);
pub const SPIDER_VEHICLE_DRAW: GameFn =
    f("?draw@SpiderVehicle@@UEBAXAEAVDrawQueue@@@Z", 0x0051ECA0);
pub const UNIT_DRAW: GameFn = f("?draw@Unit@@UEBAXAEAVDrawQueue@@@Z", 0x0055FC50);
pub const SPIDER_UNIT_DRAW: GameFn = f("?draw@SpiderUnit@@UEBAXAEAVDrawQueue@@@Z", 0x00519AF0);
pub const SEGMENTED_UNIT_DRAW: GameFn =
    f("?draw@SegmentedUnit@@UEBAXAEAVDrawQueue@@@Z", 0x00574BA0);
pub const SEGMENT_DRAW: GameFn = f("?draw@Segment@@UEBAXAEAVDrawQueue@@@Z", 0x00572F80);
pub const COMBAT_ROBOT_DRAW: GameFn = f("?draw@CombatRobot@@UEBAXAEAVDrawQueue@@@Z", 0x00391240);
pub const THRUSTER_DRAW: GameFn = f("?draw@Thruster@@UEBAXAEAVDrawQueue@@@Z", 0x005346C0);
// logistic + construction bots share this base-class draw (found via pdb-explorer)
pub const ROBOT_LOGISTIC_DRAW: GameFn =
    f("?draw@RobotWithLogisticInterface@@UEBAXAEAVDrawQueue@@@Z", 0x004D61E0);

// laid flat but lifted onto a low platform (their top-down art reads wrong
// standing up)
pub const SOLAR_PANEL_DRAW: GameFn = f("?draw@SolarPanel@@UEBAXAEAVDrawQueue@@@Z", 0x00503250);
pub const ROCKET_SILO_DRAW: GameFn = f("?draw@RocketSilo@@UEBAXAEAVDrawQueue@@@Z", 0x004DEB80);

// --- direction-based entity draws (direction byte rotated during draw) -------
pub const SPLITTER_DRAW_BASE: GameFn = f("?drawBase@Splitter@@", 0x00528CE0);
pub const LANE_SPLITTER_DRAW_BASE: GameFn = f("?drawBase@LaneSplitter@@", 0x0044E490);
pub const UG_BELT_DRAW_BASE: GameFn = f("?drawBase@UndergroundBelt@@", 0x005589E0);
pub const INSERTER_DRAW: GameFn = f("?draw@Inserter@@UEBAXAEAVDrawQueue@@@Z", 0x0042DF50);
pub const PIPE_TO_GROUND_DRAW: GameFn = f("?draw@PipeToGround@@UEBAXAEAVDrawQueue@@@Z", 0x004830C0);
pub const PIPE_DRAW: GameFn = f("?draw@Pipe@@UEBAXAEAVDrawQueue@@@Z", 0x00480D10);
// pauses draw-time direction edits while the map serializes (desync guard)
pub const MAP_SAVE: GameFn = f("?save@Map@@QEBAXAEAVMapSerialiser@@", 0x00B5ED30);

// --- camera-facing frame selection (which rotation frame gets drawn) ---------
pub const RS_DIR_PIC_INDEX: GameFn = f("?getDirectionPictureIndex@RotatedSprite@@", 0x0068FC70);
pub const ARI_FRAME_INDEX: GameFn =
    f("?getFrameIndexForOrientation@AnimationRotationIndex@@", 0x00F55660);
pub const SPRITE_NWAY4_DRAW: GameFn = f("?draw@?$SpriteNWay@$03@@", 0x01085F80);
pub const PIPE_GET_SPRITE_GROUP: GameFn = f("?getSpriteGroup@Pipe@@", 0x00480B80);

// every function above, for the pdb scan
pub const ALL: &[&GameFn] = &[
    &ENTITY_RENDERER_PREPARE,
    &GAME_RENDERER_RENDER,
    &GAME_RENDERER_ACK_RESIZE,
    &CREATE_RENDER_PARAMS,
    &RP_CENTER_ON,
    &GCEF_SET_SIZE,
    &DRAW_ENTITIES,
    &DCB_FLUSH,
    &DQ_DRAW_VEC,
    &DQ_DRAW_PAIR,
    &DQ_DRAW_TINTED,
    &DQ_DRAW_ROTATED,
    &DQ_DRAW_ROTATED_TINTED,
    &DQ_DRAW_ROTATED_WITHOUT_TINT,
    &DQ_DRAW_SHIFTED_ROT,
    &DQ_DRAW_SHIFTED_ROT_PRECISE,
    &DQ_DRAW_SCALED_ROTATED,
    &PLACE_SPRITE,
    &COMPUTE_WALK_DIR,
    &GET_MAP_POSITION,
    &CHARACTER_DRAW,
    &CAR_DRAW,
    &LOCOMOTIVE_DRAW,
    &CARGO_WAGON_DRAW,
    &ROLLING_STOCK_DRAW,
    &SPIDER_VEHICLE_DRAW,
    &UNIT_DRAW,
    &SPIDER_UNIT_DRAW,
    &SEGMENTED_UNIT_DRAW,
    &SEGMENT_DRAW,
    &COMBAT_ROBOT_DRAW,
    &ROBOT_LOGISTIC_DRAW,
    &SOLAR_PANEL_DRAW,
    &ROCKET_SILO_DRAW,
    &THRUSTER_DRAW,
    &SPLITTER_DRAW_BASE,
    &LANE_SPLITTER_DRAW_BASE,
    &UG_BELT_DRAW_BASE,
    &INSERTER_DRAW,
    &PIPE_TO_GROUND_DRAW,
    &PIPE_DRAW,
    &MAP_SAVE,
    &RS_DIR_PIC_INDEX,
    &ARI_FRAME_INDEX,
    &SPRITE_NWAY4_DRAW,
    &PIPE_GET_SPRITE_GROUP,
];

// --- struct field offsets (2.0.77, reverse-engineered) -----------------------

// RenderParameters fields
pub const RP_WIDTH: usize = 0x168; // u16, render width in px
pub const RP_HEIGHT: usize = 0x16A; // u16, render height in px
pub const RP_SCALE: usize = 0x170; // f64, displayScale * zoom
pub const RP_RECT: usize = 0x180; // 4x i32 view rect, 1/256-tile fixed point

// the view rect's fixed-point divisor (1/256 tile per unit)
pub const RECT_FP: f32 = 256.0;
// pixels per tile at RenderParameters scale 1.0
pub const PX_PER_TILE_SCALE_1: f64 = 32.0;

// sprite flags (u16 at Sprite+0x54): bits 2/4 mark shadow/light sprites,
// which render into their own framebuffers and must not become billboards
pub const SPRITE_FLAGS: usize = 0x54;
pub const SPRITE_SHADOW_OR_LIGHT_BITS: u16 = 0x6;

// SpriteDrawParameters fields (filled by the placement helper)
pub const PARAM_CENTER_X: usize = 0x10; // f32, on-screen center x (fbo px)
pub const PARAM_CENTER_Y: usize = 0x14; // f32, on-screen center y
pub const PARAM_SCALE_X: usize = 0x24; // f32, on-screen scale x
pub const PARAM_SCALE_Y: usize = 0x28; // f32, on-screen scale y
pub const PARAM_TINT: usize = 0x2C; // u32, rgba8 tint (0 = invisible)
pub const PARAM_SIZE_X: usize = 0x38; // f32, sprite px size
pub const PARAM_SIZE_Y: usize = 0x3C; // f32, sprite px size

// direction byte inside TransportBeltConnectable (splitters, ug belts)
pub const TBC_DIR_OFF: usize = 0x128;
// direction byte inside Inserter / PipeToGround
pub const INSERTER_DIR_OFF: usize = 0x1D8;

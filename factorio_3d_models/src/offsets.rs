// everything specific to one factorio build (2.0.77) lives here.
//
// each hooked game function has a pdb symbol-name fragment and a fallback
// rva (offset from the exe base). functions are found by NAME first, so most
// hooks survive game updates; the rvas are only used when the symbol is
// missing.

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
pub const GAME_RENDERER_RENDER: GameFn = f("?render@GameRenderer@@", 0x0065B3C0);
pub const GAME_RENDERER_ACK_RESIZE: GameFn = f("?acknowledgeResize@GameRenderer@@", 0x0065B830);
pub const CREATE_RENDER_PARAMS: GameFn = f("?createRenderParameters@GameView@@", 0x0010B7F0);
// centerOn writes the real scale (displayScale x zoom); the horizon boost
// divides the zoom around this call
pub const RP_CENTER_ON: GameFn = f("?centerOn@RenderParameters@@", 0x00682A20);

// --- player / input -----------------------------------------------------------
pub const COMPUTE_WALK_DIR: GameFn =
    f("?computeWalkingDirectionFromPlayerInputs@PlayerInputSource@@", 0x00A95B90);
pub const GET_MAP_POSITION: GameFn = f(
    "?getMapPosition@GameView@@QEBA?AVMapPosition@@VPixelPosition@@@Z",
    0x0010E760,
);

// --- replaced entities ----------------------------------------------------------
pub const ASSEMBLING_MACHINE_DRAW: GameFn =
    f("?draw@AssemblingMachine@@UEBAXAEAVDrawQueue@@@Z", 0x00322BC0);
pub const FURNACE_DRAW: GameFn = f("?draw@Furnace@@UEBAXAEAVDrawQueue@@@Z", 0x0040BA30);
pub const MINING_DRILL_DRAW: GameFn = f("?draw@MiningDrill@@UEBAXAEAVDrawQueue@@@Z", 0x0046E810);
pub const TURRET_DRAW: GameFn = f("?draw@Turret@@UEBAXAEAVDrawQueue@@@Z", 0x0054D720);
pub const AMMO_TURRET_DRAW: GameFn = f("?draw@AmmoTurret@@UEBAXAEAVDrawQueue@@@Z", 0x003105B0);
// ~Entity — the end of every entity destruction chain (registry cleanup)
pub const ENTITY_DTOR: GameFn = f("??1Entity@@UEAA@XZ", 0x003C3040);
// draws a crafting machine's sprites (animation, shadow, working
// visualisations); receives the animation state, a status enum, the map
// position and the direction — everything the 3d replacement needs
pub const WV_DRAW_CRAFTING_MACHINE: GameFn =
    f("?drawCraftingMachine@WorkingVisualisations@@", 0x006D9CF0);
// same idea for mining drills
pub const WV_DRAW_MINING_DRILL: GameFn =
    f("?drawMiningDrill@WorkingVisualisations@@", 0x006DA0A0);
// Entity::getPrototype() -> EntityPrototype* (called, not hooked)
pub const ENTITY_GET_PROTOTYPE: GameFn =
    f("?getPrototype@Entity@@QEBAPEBVEntityPrototype@@XZ", 0x0003E610);
// Entity::getSurface() -> Surface& — space-age platform views draw through
// the same hooks; entities are tagged so only the main surface renders
pub const ENTITY_GET_SURFACE: GameFn =
    f("?getSurface@Entity@@QEBAAEBVSurface@@XZ", 0x0003E880);
// float DayTime::getDarkness(double) const — called (never hooked) once a
// frame to cache the live darkness for the day/night sync
pub const DAYTIME_GET_DARKNESS: GameFn = f("?getDarkness@DayTime@@", 0x00B4C9B0);
// ItemStackPainter::drawItemStack(DrawQueue&, uint, Sprite const&, MapPosition,
//   float, Color, RenderLayer, RenderLayer, char, FixedPoint) — every item on
// a belt (or the ground) draws through this; gives position + sprite
pub const DRAW_ITEM_STACK: GameFn = f("?drawItemStack@ItemStackPainter@@", 0x001E6E70);
// belt entities never pass through their ::draw during chunk rendering (the
// belt surface is drawn by a cached animation set) — but belts CARRYING
// ITEMS call drawItems every frame. these are the belt recorders
pub const TB_DRAW_ITEMS: GameFn = f("?drawItems@TransportBelt@@", 0x00542880);
// DrawQueue::drawScaledTinted(Sprite const&, MapPosition const&, double,
// double, Color, DrawingFlags, RenderLayer, Vector const&, char) — the fast
// sprite path normal belt items take (they never reach drawInternal). hooked
// only while a belt is drawing (IN_BELT_DRAW); the sprite is mapped back to
// its item via a prebuilt belt-sprite -> model table
pub const DRAW_SCALED_TINTED: GameFn = f("?drawScaledTinted@DrawQueue@@", 0x00649070);
// Inserter member fields (disasm of Inserter::draw @0x42DF50, 2.0.77): +0x110 =
// embedded held ItemStack; the item id is the u16 at +0x114 (0 = empty hand —
// the game tests `word [inserter+0x114]` to pick the open/closed hand sprite).
// +0x1e8 = arm swing angle (RealOrientation, float) fed to the sin/cos that
// places the hand; drives both the held-item position and the arm animation
pub const INSERTER_HELD_ITEM_ID: usize = 0x114;
pub const INSERTER_ARM_ANGLE: usize = 0x1e8;
// InserterPrototype rotation_speed (f64): 0.014 base, 0.04 fast/bulk. drives
// the per-tier arm animation speed. base value used as the reference
pub const INSERTER_ROTATION_SPEED: usize = 0x6E0;
pub const INSERTER_BASE_ROTATION: f32 = 0.014;
// ItemPrototype belt-sprite variation range: [+0x368, +0x370), 0xA0 stride.
// only ~7 items (coal, ores, uranium) have this belt-pictures array
pub const ITEM_PROTO_SPRITE_BASE: usize = 0x368;
pub const ITEM_PROTO_SPRITE_END: usize = 0x370;
pub const ITEM_SPRITE_STRIDE: usize = 0xA0;
// every OTHER item draws its icon on the belt: the on-belt Sprite lives at
// *(proto + 0x2B8) + 0x68 (reverse-scanned from plastic-bar/solid-fuel/sulfur,
// all three the same). this is what covers plates, pipes, building parts...
pub const ITEM_PROTO_ICON_FIELD: usize = 0x2B8;
pub const ITEM_PROTO_ICON_SPRITE_OFF: usize = 0x68;
pub const UB_DRAW_ITEMS: GameFn = f("?drawItems@UndergroundBelt@@", 0x00558CF0);
pub const SPLITTER_DRAW_ITEMS: GameFn = f("?drawItems@Splitter@@", 0x005297E0);
// TransportBeltConnectable::getDirection — belt direction for the recorders
pub const TBC_GET_DIRECTION: GameFn =
    f("?getDirection@TransportBeltConnectable@@", 0x000735F0);
// items dropped on the ground are entities; the drawItemStack call inside
// their draw identifies which item they hold
pub const ITEM_ENTITY_DRAW: GameFn = f("?draw@ItemEntity@@UEBAXAEAVDrawQueue@@@Z", 0x0043D6E0);
// Item::drawItemOnMap(DrawQueue&, MapPosition const&, RenderLayer, char,
// Item const&) — belt items draw through here (TransportLine::draw), NOT
// through drawItemStack
pub const ITEM_DRAW_ON_MAP: GameFn = f("?drawItemOnMap@Item@@", 0x00ABF5F0);
// ItemStack::drawInternal(DrawQueue&, MapPosition const&, uchar, RenderLayer,
// RenderLayer, char, FixedPoint) — the stacked/qualified item draw path
pub const ITEM_DRAW_INTERNAL: GameFn = f("?drawInternal@ItemStack@@", 0x00ACA580);
// WireRendering::draw(DrawQueue&, MapPosition from, MapPosition to, double
// height, NamedBool<WireShadowTag>, Sprite const&, Color const&, RenderLayer)
// — every wire span (copper + circuit) funnels through this
pub const WIRE_DRAW_SEGMENT: GameFn =
    f("?draw@WireRendering@@SAXAEAVDrawQueue@@AEBVMapPosition@@", 0x00FFEE80);
// WireRendering::drawWires(DrawQueue&, WireConnector const&, WireStyle) —
// called manually for suppressed poles so their wires still submit
pub const WIRE_DRAW_WIRES: GameFn = f("?drawWires@WireRendering@@", 0x00FFE7C0);
// ElectricPole's WireConnector member: `lea rdx,[rbx+0xB8]` right before the
// drawWires call at rva 0x3B91B8 inside ?draw@ElectricPole@@ (2.0.77)
pub const POLE_WIRE_CONNECTOR_OFF: usize = 0xB8;
// EntityWithHealth::die — destroyed buildings drop their 3d model right
// away (the remnant sprite takes over); ~Entity may run much later
pub const EWH_DIE: GameFn = f("?die@EntityWithHealth@@", 0x003EB660);
// GUI entity-preview buttons (silo menu etc.) render entities through their
// real ::draw — bracket these so our hooks pass through and never suppress /
// record a menu preview (keeps the vanilla 2d rocket in the silo GUI)
pub const ENTITY_BUTTON_DRAW_A: GameFn = f(
    "?drawEntity@?$EntityButtonBase@VEmptyWidget@agui@@@@MEAAXAEAVDrawQueue@@@Z",
    0x011AB2F0,
);
pub const ENTITY_BUTTON_DRAW_B: GameFn = f(
    "?drawEntity@?$EntityButtonBase@VButton@agui@@@@MEAAXAEAVDrawQueue@@@Z",
    0x011AC030,
);
// the rocket inside a silo: its own entity, drawn rising via cropped sprites
pub const ROCKET_DRAW: GameFn =
    f("?draw@RocketSiloRocket@@UEBAXAEAVDrawQueue@@@Z", 0x004E8D90);
// RocketSiloRocket::drawRocketCroppedSprite(DrawQueue&, Sprite const&,
// RealOrientation const&, Vector const& offset, char, RenderLayer, float,
// SpidertronLayeringFix, DrawingFlags) — the offset carries the rise height
pub const ROCKET_CROPPED_SPRITE: GameFn =
    f("?drawRocketCroppedSprite@RocketSiloRocket@@", 0x004E8B50);
// BuildingRenderer::drawEntityToBeBuilt(GameView const&, Player const*,
//   NamedBool<GhostModeTag>, BuildMode, MapPosition const&, EntityPrototype
//   const*, Controller const&, IDWithQuality<ItemPrototype>, DrawQueue&)
//   -> ItemToBuildDrawnType. the build-cursor placement preview: arg5 = build
//   position, arg6 = the prototype being placed. the "CA" (static cdecl, const
//   GameView) picks this overload over the higher-level "SA" one
pub const DRAW_ENTITY_TO_BE_BUILT: GameFn =
    f("?drawEntityToBeBuilt@BuildingRenderer@@CA", 0x005E3490);
// EntityWithHealth::getDeconstructionMarkerPosition() -> MapPosition in rax
// (turrets derive from EntityWithHealth; used as a generic position getter)
pub const EWH_DECON_MARKER_POS: GameFn =
    f("?getDeconstructionMarkerPosition@EntityWithHealth@@", 0x0003EAF0);
// Turret::getOrientation() -> RealOrientation in rax (head rotation, 0..1)
pub const TURRET_GET_ORIENTATION: GameFn =
    f("?getOrientation@Turret@@UEBA?AVRealOrientation@@XZ", 0x000D6380);
// Entity::getDirection() -> Direction (building rotation, 0..16)
pub const ENTITY_GET_DIRECTION: GameFn =
    f("?getDirection@Entity@@UEBA?AVDirection@@XZ", 0x00034460);
// Entity::getOrientation() -> RealOrientation (vehicles update this field)
pub const ENTITY_GET_ORIENTATION: GameFn =
    f("?getOrientation@Entity@@UEBA?AVRealOrientation@@XZ", 0x0003E740);

// --- the player character ---------------------------------------------------
// Character::drawInternal(DrawQueue&, bool latency) — suppressed like the
// other entity draws; the 3d player model (per-state glbs under
// ENTITIES/PLAYER) takes over. hooked INSTEAD of Character::draw because on a
// multiplayer client the local player's visible character is the LatencyState
// copy: Character::draw early-outs for the duplicated game-state character
// and EntityRenderer::prepareRow draws the latency copy by calling
// drawInternal directly with latency=true — a ::draw hook never sees it.
// every other path (::draw tail-call, GUI entity buttons) funnels through
// drawInternal too, so this one hook covers them all.
pub const CHARACTER_DRAW_INTERNAL: GameFn =
    f("?drawInternal@Character@@AEBAXAEAVDrawQueue@@_N@Z", 0x00373BD0);
// Character::getDirection() -> Direction (the 8-way facing the sprite uses)
pub const CHARACTER_GET_DIRECTION: GameFn =
    f("?getDirection@Character@@UEBA?AVDirection@@XZ", 0x0037BC60);
// Character::getPlayerColor() -> Color (the /color player color) — tints the
// 3d model the same way the vanilla sprite is tinted
pub const CHARACTER_GET_PLAYER_COLOR: GameFn =
    f("?getPlayerColor@Character@@QEBA?AVColor@@XZ", 0x00380990);
// bool ManualMiner::performMining() — runs every tick while the local player
// holds mine on a target; the mining-pose heartbeat
pub const MANUAL_MINER_PERFORM: GameFn = f("?performMining@ManualMiner@@QEAA_NXZ", 0x0015BDB0);
// static bool ShooterLogic::shoot<Character>(Character&) — a character firing
// its gun funnels through this; the shooting-pose heartbeat (per character)
pub const SHOOTER_SHOOT_CHARACTER: GameFn =
    f("??$shoot@VCharacter@@@ShooterLogic@@SA_NAEAVCharacter@@@Z", 0x01279F80);

// --- generic building / vehicle draws --------------------------------------------
// every class here gets the same treatment: record position + direction (or
// smooth orientation for vehicles), suppress the vanilla sprite once the
// model is loaded.
//
// getDirection/getOrientation are virtual and overridden per class — a call
// to the Entity base impl returns north/0. dir_sym/orient_sym name the
// class's own override ("" = the class doesn't override, base is correct)
pub struct DrawTarget {
    pub gf: GameFn,
    pub oriented: bool, // yaw from RealOrientation + animate while moving
    pub anim_on_draw: bool, // play the glb animation whenever the chunk redraws
    pub dir_sym: &'static str,
    pub orient_sym: &'static str,
    // getRelativeTurretOrientation override (tank) — plain float return
    pub turret_sym: &'static str,
    // activity-progress getter (inserters) — double return, drives the
    // fingerprint so the glb animation plays exactly while the arm moves
    pub progress_sym: &'static str,
    // WireConnector offset inside the entity (poles): when the draw is
    // suppressed, drawWires is called manually so the wires still submit
    pub wire_off: usize,
    // activity-rate getter (accumulator) — double return; NONZERO value =
    // the entity is actively working (gates the arc prims), unlike
    // progress_sym where a CHANGING value means working
    pub activity_sym: &'static str,
    // gate openingProgress float offset inside the entity (0 = not a gate).
    // 1.0 = fully closed, 0.0 = fully open — the model's shapekey is the
    // inverse (0 = extended/closed, 1 = retracted), so morph = 1 - progress
    pub gate_off: usize,
}

const BASE: DrawTarget = DrawTarget {
    gf: f("", 0),
    oriented: false,
    anim_on_draw: false,
    dir_sym: "",
    orient_sym: "",
    turret_sym: "",
    progress_sym: "",
    wire_off: 0,
    activity_sym: "",
    gate_off: 0,
};

const fn d(symbol: &'static str, rva: usize) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), ..BASE }
}

// building with its own getDirection override
const fn dd(symbol: &'static str, rva: usize, dir_sym: &'static str) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), dir_sym, ..BASE }
}

// dd + an activity-progress getter driving the animation (inserters)
const fn dp(
    symbol: &'static str,
    rva: usize,
    dir_sym: &'static str,
    progress_sym: &'static str,
) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), dir_sym, progress_sym, ..BASE }
}

// building that owns wires (poles): connector offset for the manual submit
const fn dw(symbol: &'static str, rva: usize, wire_off: usize) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), wire_off, ..BASE }
}

// building whose redraw means "working" (radar: powered = animating = redrawn
// every frame; the fingerprint is the frame counter so redraws show as work)
const fn dan(symbol: &'static str, rva: usize) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), anim_on_draw: true, ..BASE }
}

// d + an activity-rate getter (accumulator: nonzero = charging/discharging)
const fn da(symbol: &'static str, rva: usize, activity_sym: &'static str) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), activity_sym, ..BASE }
}

// the gate: dd + the openingProgress field driving the shapekey morph
const fn dg(
    symbol: &'static str,
    rva: usize,
    dir_sym: &'static str,
    gate_off: usize,
) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), dir_sym, gate_off, ..BASE }
}

// vehicle with a getOrientation override
const fn dv(symbol: &'static str, rva: usize, orient_sym: &'static str) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), oriented: true, orient_sym, ..BASE }
}

// dv + a relative turret orientation getter (the tank)
const fn dvt(
    symbol: &'static str,
    rva: usize,
    orient_sym: &'static str,
    turret_sym: &'static str,
) -> DrawTarget {
    DrawTarget { gf: f(symbol, rva), oriented: true, orient_sym, turret_sym, ..BASE }
}

pub const GENERIC_DRAWS: &[DrawTarget] = &[
    d("?draw@Lab@@UEBAXAEAVDrawQueue@@@Z", 0x00445450),
    dd("?draw@Boiler@@UEBAXAEAVDrawQueue@@@Z", 0x003431D0, "?getDirection@Boiler@@"),
    dd("?draw@Generator@@UEBAXAEAVDrawQueue@@@Z", 0x0041EBC0, "?getDirection@Generator@@"),
    // the activity progress changes exactly while the arm swings — that's
    // the fingerprint, so the glb animation plays only when working
    dp(
        "?draw@Inserter@@UEBAXAEAVDrawQueue@@@Z",
        0x0042DF50,
        "?getDirection@Inserter@@",
        "?getActivityProgress@Inserter@@",
    ),
    dw("?draw@ElectricPole@@UEBAXAEAVDrawQueue@@@Z", 0x003B90F0, POLE_WIRE_CONNECTOR_OFF),
    d("?draw@ContainerEntity@@UEBAXAEAVDrawQueue@@@Z", 0x0039F030),
    d("?draw@LogisticContainer@@UEBAXAEAVDrawQueue@@@Z", 0x00466170),
    d("?draw@SolarPanel@@UEBAXAEAVDrawQueue@@@Z", 0x00503250),
    // arcs (the emissive lightning prim) only draw while the activity rate
    // says the accumulator is actually charging/discharging
    da(
        "?draw@Accumulator@@UEBAXAEAVDrawQueue@@@Z",
        0x002FD740,
        "?getActivityRate@Accumulator@@",
    ),
    d("?draw@Beacon@@UEBAXAEAVDrawQueue@@@Z", 0x00339980),
    d("?draw@Roboport@@UEBAXAEAVDrawQueue@@@Z", 0x004CDBC0),
    dan("?draw@Radar@@UEBAXAEAVDrawQueue@@@Z", 0x00499120),
    d("?draw@Wall@@UEBAXAEAVDrawQueue@@@Z", 0x0056D740),
    // the gate: openingProgress (disasm of Gate::draw @0x41AC20: movss
    // xmm3,[entity+0x104], frame = (1-progress)*frames) drives the shapekey
    dg(
        "?draw@Gate@@UEBAXAEAVDrawQueue@@@Z",
        0x0041AC20,
        "?getDirection@Gate@@",
        GATE_OPENING_PROGRESS,
    ),
    dd("?draw@StorageTank@@UEBAXAEAVDrawQueue@@@Z", 0x00531920, "?getDirection@StorageTank@@"),
    dd("?draw@Pump@@UEBAXAEAVDrawQueue@@@Z", 0x004938D0, "?getDirection@Pump@@"),
    dd(
        "?draw@OffshorePump@@UEBAXAEAVDrawQueue@@@Z",
        0x0047B4A0,
        "?getDirection@OffshorePump@@",
    ),
    d("?draw@Reactor@@UEBAXAEAVDrawQueue@@@Z", 0x004C30E0),
    dd("?draw@TrainStop@@UEBAXAEAVDrawQueue@@@Z", 0x0053C9C0, "?getDirection@TrainStop@@"),
    d("?draw@Lamp@@UEBAXAEAVDrawQueue@@@Z", 0x00449F00),
    d("?draw@ProgrammableSpeaker@@UEBAXAEAVDrawQueue@@@Z", 0x00489770),
    dd("?draw@ArithmeticCombinator@@UEBAXAEAVDrawQueue@@@Z", 0x00312330, "?getDirection@Combinator@@"),
    dd("?draw@DeciderCombinator@@UEBAXAEAVDrawQueue@@@Z", 0x003B12F0, "?getDirection@Combinator@@"),
    dd("?draw@ConstantCombinator@@UEBAXAEAVDrawQueue@@@Z", 0x003965E0, "?getDirection@ConstantCombinator@@"),
    d("?draw@PowerSwitch@@UEBAXAEAVDrawQueue@@@Z", 0x00487420),
    // ore patches / oil wells (folders under ENTITIES/RESOURCES)
    d("?draw@ResourceEntity@@UEBAXAEAVDrawQueue@@@Z", 0x004C7EC0),
    // trees -> ENTITIES/FOLIAGE models (mapped tree-01.. -> tree1..tree5)
    d("?draw@Tree@@UEBAXAEAVDrawQueue@@@Z", 0x00549EF0),
    // rocks/stones are SimpleEntity prototypes (big-rock, huge-rock,
    // big-sand-rock, ...) — mapped onto the ground-clutter rock model
    d("?draw@SimpleEntity@@UEBAXAEAVDrawQueue@@@Z", 0x004FBF30),
    // connectables: variants picked from the neighbor grid in entities.rs
    d("?draw@Pipe@@UEBAXAEAVDrawQueue@@@Z", 0x00480D10),
    dd("?draw@PipeToGround@@UEBAXAEAVDrawQueue@@@Z", 0x004830C0, "?getDirection@PipeToGround@@"),
    d("?draw@HeatPipe@@UEBAXAEAVDrawQueue@@@Z", 0x00423940),
    // TBC::draw only runs for hover/selection redraws; the belt SURFACE at
    // chunk render goes through each class's drawBase — hook those too, or
    // belts only get their 3d model after the first mouse-over
    dd(
        "?draw@TransportBeltConnectable@@UEBAXAEAVDrawQueue@@@Z",
        0x005459F0,
        "?getDirection@TransportBeltConnectable@@",
    ),
    dd(
        "?drawBase@TransportBelt@@UEBAXAEAVDrawQueue@@@Z",
        0x00542540,
        "?getDirection@TransportBeltConnectable@@",
    ),
    dd(
        "?drawBase@UndergroundBelt@@UEBAXAEAVDrawQueue@@@Z",
        0x005589E0,
        "?getDirection@TransportBeltConnectable@@",
    ),
    dd(
        "?drawBase@Splitter@@UEBAXAEAVDrawQueue@@@Z",
        0x00528CE0,
        "?getDirection@TransportBeltConnectable@@",
    ),
    // vehicles: smooth orientation, animate while moving
    dvt(
        "?draw@Car@@UEBAXAEAVDrawQueue@@@Z",
        0x00351640,
        "?getOrientation@Vehicle@@",
        "?getRelativeTurretOrientation@Car@@",
    ),
    dv("?draw@Locomotive@@UEBAXAEAVDrawQueue@@@Z", 0x00461A30, "?getOrientation@Vehicle@@"),
    dv("?draw@CargoWagon@@UEBAXAEAVDrawQueue@@@Z", 0x003695D0, "?getOrientation@Vehicle@@"),
    dv("?draw@RollingStock@@UEBAXAEAVDrawQueue@@@Z", 0x004F3AB0, "?getOrientation@Vehicle@@"),
    dv(
        "?draw@RobotWithLogisticInterface@@UEBAXAEAVDrawQueue@@@Z",
        0x004D61E0,
        "?getOrientation@RobotWithLogisticInterface@@",
    ),
    // enemies: biters are the Unit class. suppress the vanilla sprite, take
    // the smooth facing from Unit::getOrientation, and play the walk clip while
    // moving. rva 0 = resolved by symbol only (the pdb is required anyway);
    // spitters are Units too but keep their sprite (no biter_* model matches),
    // worms are Turrets and are already hooked above.
    dv(
        "?draw@Unit@@UEBAXAEAVDrawQueue@@@Z",
        0x0,
        "?getOrientation@Unit@@UEBA?AVRealOrientation@@XZ",
    ),
];

// every function above, for the pdb scan
pub const ALL: &[&GameFn] = &[
    &GAME_RENDERER_RENDER,
    &GAME_RENDERER_ACK_RESIZE,
    &CREATE_RENDER_PARAMS,
    &RP_CENTER_ON,
    &COMPUTE_WALK_DIR,
    &GET_MAP_POSITION,
    &ASSEMBLING_MACHINE_DRAW,
    &FURNACE_DRAW,
    &MINING_DRILL_DRAW,
    &TURRET_DRAW,
    &AMMO_TURRET_DRAW,
    &ENTITY_DTOR,
    &WV_DRAW_CRAFTING_MACHINE,
    &WV_DRAW_MINING_DRILL,
    &ENTITY_GET_PROTOTYPE,
    &ENTITY_GET_SURFACE,
    &DRAW_ITEM_STACK,
    &TB_DRAW_ITEMS,
    &DRAW_SCALED_TINTED,
    &UB_DRAW_ITEMS,
    &SPLITTER_DRAW_ITEMS,
    &TBC_GET_DIRECTION,
    &ITEM_ENTITY_DRAW,
    &ITEM_DRAW_ON_MAP,
    &ITEM_DRAW_INTERNAL,
    &WIRE_DRAW_SEGMENT,
    &WIRE_DRAW_WIRES,
    &EWH_DIE,
    &ENTITY_BUTTON_DRAW_A,
    &ENTITY_BUTTON_DRAW_B,
    &ROCKET_DRAW,
    &ROCKET_CROPPED_SPRITE,
    &DRAW_ENTITY_TO_BE_BUILT,
    &EWH_DECON_MARKER_POS,
    &TURRET_GET_ORIENTATION,
    &ENTITY_GET_DIRECTION,
    &ENTITY_GET_ORIENTATION,
    &DAYTIME_GET_DARKNESS,
    &CHARACTER_DRAW_INTERNAL,
    &CHARACTER_GET_DIRECTION,
    &CHARACTER_GET_PLAYER_COLOR,
    &MANUAL_MINER_PERFORM,
    &SHOOTER_SHOOT_CHARACTER,
];

// --- struct field offsets (2.0.77, reverse-engineered) -----------------------

// RenderParameters fields
pub const RP_WIDTH: usize = 0x168; // u16, render width in px
pub const RP_SCALE: usize = 0x170; // f64, displayScale * zoom
pub const RP_RECT: usize = 0x180; // 4x i32 view rect, 1/256-tile fixed point

// the view rect's fixed-point divisor (1/256 tile per unit)
pub const RECT_FP: f32 = 256.0;
// CraftingMachineAnimationState (disasm of ::update @ rva 0x3AE740):
// last-update MapTick + current frame — together the working fingerprint
pub const CM_ANIM_TICK: usize = 0x10;
pub const CM_ANIM_FRAME: usize = 0x4;
// drawMiningDrill's AnimState is a DIFFERENT struct: these two dwords climb
// ~1/frame while the drill works and freeze when it idles (verified in-game)
pub const MD_ANIM_LO: usize = 0x0;
pub const MD_ANIM_HI: usize = 0x8;
// item id (u16) inside an ItemStack
pub const ITEMSTACK_ID: usize = 0x4;
// Entity base map-position field (disasm of ?renderPosition@Entity@@ 0x3E670:
// `mov rax,[rcx+0x50]`) — a MapPosition (2x i32, 1/256 fixed point). safe to
// read on ANY entity, unlike the EntityWithHealth getter
pub const ENTITY_POS_FIELD: usize = 0x50;
// the cursor's build Direction byte inside GameView (disasm of
// updateBuildDirectionAfterPipette @0x10A9B0: the rotate/flip arithmetic
// reads and writes `byte [gameview+8]`) — rotates the 3d build-cursor ghost.
// drawEntityToBeBuilt @0x5E35A3 prefers an OVERRIDE direction when its flag
// is set: b = [[gameview+0x30]+0x1C0]; dir = [b+0x58] if [b+0x59] else
// [gameview+8] — replicated in the e2b hook
pub const GAMEVIEW_BUILD_DIR: usize = 0x8;
pub const GAMEVIEW_DIR_CHAIN_A: usize = 0x30;
pub const GAMEVIEW_DIR_CHAIN_B: usize = 0x1C0;
pub const DIR_OVERRIDE_VALUE: usize = 0x58;
pub const DIR_OVERRIDE_FLAG: usize = 0x59;
// Gate openingProgress (float): 1.0 = fully closed, 0.0 = fully open,
// stepped by opening_speed each tick in Gate::update (rcx there is the
// UpdatableEntity base at entity+0xD8, the field is its +0x2C)
pub const GATE_OPENING_PROGRESS: usize = 0x104;
// ghost-mode byte inside DrawQueue (set by DrawQueue::DrawGhostGuard while
// the build-cursor preview / ghost sprites draw; disasm of ??1DrawGhostGuard
// @ rva 0x408A0: mov [drawqueue+0x12A0], saved_byte)
pub const DQ_GHOST_MODE: usize = 0x12A0;
// pixels per tile at RenderParameters scale 1.0
pub const PX_PER_TILE_SCALE_1: f64 = 32.0;

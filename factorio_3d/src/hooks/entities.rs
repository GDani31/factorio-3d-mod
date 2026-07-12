// entity draw-method hooks.
//
// players, vehicles, trains and units queue their sprites through paths that
// never set a DrawQueue layer, so each class's draw() is bracketed with
// thread-local flags — the placement hook then records their sprites.
// direction-based buildings (splitters, inserters, pipes) pick their sprite
// inline from a direction byte; that byte is rotated for the duration of the
// draw call only, so the camera yaw shows the matching side.

use crate::offsets::{self, GameFn};
use crate::symbols::SymbolMap;
use anyhow::Result;
use retour::static_detour;
use std::sync::atomic::{AtomicBool, Ordering};

type FnEntityDraw = unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
type EntityDrawDetour =
    retour::StaticDetour<unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void)>;

static_detour! {
    static CharacterDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static CarDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static LocomotiveDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static CargoWagonDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static RollingStockDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static SpiderVehicleDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static UnitDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static SpiderUnitDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static SegmentedUnitDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static SegmentDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static CombatRobotDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static RobotLogisticDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
}

// AgriculturalCrane::draw(DrawQueue&, NamedBool<GhostModeTag>) — the ghost flag
// is a 1-byte struct passed in R8B
static_detour! {
    static CraneDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, u8);
}

static_detour! {
    static SplitterDrawBaseHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static LaneSplitterDrawBaseHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static UgBeltDrawBaseHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static InserterDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static PipeToGroundDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static PipeDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static ThrusterDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static SolarPanelDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static RocketSiloDrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void);
    static MapSaveHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, *mut core::ffi::c_void);
}

// true while Map::save runs. direction-byte edits pause during it: the
// serializer runs concurrently with draw prepare, and saving a temporarily
// rotated inserter causes a desync error
static SAVE_ACTIVE: AtomicBool = AtomicBool::new(false);

// --- player -----------------------------------------------------------------------

fn hooked_character_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    // tag the sprites emitted inside as the player's, under one entity serial
    let s = super::ENTITY_SERIAL_NEXT.fetch_add(1, Ordering::Relaxed).max(1);
    super::ENTITY_DRAW_SERIAL.with(|c| c.set(s));
    super::IN_CHARACTER_DRAW.with(|f| f.set(true));
    unsafe { CharacterDrawHook.call(this, queue) }
    super::IN_CHARACTER_DRAW.with(|f| f.set(false));
    super::ENTITY_DRAW_SERIAL.with(|c| c.set(0));
}

// --- vehicles / units ----------------------------------------------------------------

// bracket a vehicle/unit draw with the entity-depth counter and one serial
// per outermost call (subclasses call their base's draw — both are hooked).
// $unit = screen-bottom anchored (compact bodies); $fly = flying robot
// (lifted + shifted south).
macro_rules! vehicle_draw {
    ($hooked:ident, $hook:ident, $unit:expr, $fly:expr) => {
        fn $hooked(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
            if $unit {
                super::IN_UNIT_DRAW.with(|d| d.set(d.get() + 1));
            }
            if $fly {
                super::IN_FLY_DRAW.with(|f| f.set(true));
            }
            let depth = super::ENTITY_DRAW_DEPTH.with(|d| {
                let v = d.get();
                d.set(v + 1);
                v
            });
            if depth == 0 {
                let s = super::ENTITY_SERIAL_NEXT.fetch_add(1, Ordering::Relaxed).max(1);
                super::ENTITY_DRAW_SERIAL.with(|c| c.set(s));
            }
            unsafe { $hook.call(this, queue) }
            super::ENTITY_DRAW_DEPTH.with(|d| {
                let v = d.get().saturating_sub(1);
                d.set(v);
                if v == 0 {
                    super::ENTITY_DRAW_SERIAL.with(|c| c.set(0));
                }
            });
            if $fly {
                super::IN_FLY_DRAW.with(|f| f.set(false));
            }
            if $unit {
                super::IN_UNIT_DRAW.with(|d| d.set(d.get().saturating_sub(1)));
            }
        }
    };
}

vehicle_draw!(hooked_car_draw, CarDrawHook, false, false);
vehicle_draw!(hooked_locomotive_draw, LocomotiveDrawHook, false, false);
vehicle_draw!(hooked_cargo_wagon_draw, CargoWagonDrawHook, false, false);
vehicle_draw!(hooked_rolling_stock_draw, RollingStockDrawHook, false, false);
vehicle_draw!(hooked_spider_vehicle_draw, SpiderVehicleDrawHook, false, false);
// enemies/units anchor at their screen bottom (compact bodies)
vehicle_draw!(hooked_unit_draw, UnitDrawHook, true, false);
vehicle_draw!(hooked_spider_unit_draw, SpiderUnitDrawHook, true, false);
vehicle_draw!(hooked_segmented_unit_draw, SegmentedUnitDrawHook, true, false);
vehicle_draw!(hooked_segment_draw, SegmentDrawHook, true, false);
vehicle_draw!(hooked_combat_robot_draw, CombatRobotDrawHook, false, true);
// logistic + construction bots (one shared base-class draw)
vehicle_draw!(hooked_robot_logistic_draw, RobotLogisticDrawHook, false, true);

// crane arm: tag its sprites (forced hi-res + lifted/south in the renderer)
fn hooked_crane_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void, ghost: u8) {
    super::IN_CRANE_DRAW.with(|f| f.set(true));
    unsafe { CraneDrawHook.call(this, queue, ghost) }
    super::IN_CRANE_DRAW.with(|f| f.set(false));
}

// player + vehicle/unit draw hooks
pub fn install_actors(symbols: &SymbolMap, base: usize) -> Result<()> {
    let addr = super::resolve(symbols, base, &offsets::CHARACTER_DRAW);
    unsafe {
        let target: FnEntityDraw = std::mem::transmute(addr);
        CharacterDrawHook.initialize(target, hooked_character_draw)?.enable()?;
    }

    let addr = super::resolve(symbols, base, &offsets::AGRICULTURAL_CRANE_DRAW);
    unsafe {
        let target: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, u8) =
            std::mem::transmute(addr);
        CraneDrawHook.initialize(target, hooked_crane_draw)?.enable()?;
    }

    let targets: [(&GameFn, &'static EntityDrawDetour, fn(_, _)); 11] = [
        (&offsets::CAR_DRAW, &CarDrawHook, hooked_car_draw as fn(_, _)),
        (&offsets::LOCOMOTIVE_DRAW, &LocomotiveDrawHook, hooked_locomotive_draw),
        (&offsets::CARGO_WAGON_DRAW, &CargoWagonDrawHook, hooked_cargo_wagon_draw),
        (&offsets::ROLLING_STOCK_DRAW, &RollingStockDrawHook, hooked_rolling_stock_draw),
        (&offsets::SPIDER_VEHICLE_DRAW, &SpiderVehicleDrawHook, hooked_spider_vehicle_draw),
        (&offsets::UNIT_DRAW, &UnitDrawHook, hooked_unit_draw),
        (&offsets::SPIDER_UNIT_DRAW, &SpiderUnitDrawHook, hooked_spider_unit_draw),
        (&offsets::SEGMENTED_UNIT_DRAW, &SegmentedUnitDrawHook, hooked_segmented_unit_draw),
        (&offsets::SEGMENT_DRAW, &SegmentDrawHook, hooked_segment_draw),
        (&offsets::COMBAT_ROBOT_DRAW, &CombatRobotDrawHook, hooked_combat_robot_draw),
        (&offsets::ROBOT_LOGISTIC_DRAW, &RobotLogisticDrawHook, hooked_robot_logistic_draw),
    ];
    for (gf, hook, detour) in targets {
        let addr = super::resolve(symbols, base, gf);
        unsafe {
            let target: FnEntityDraw = std::mem::transmute(addr);
            hook.initialize(target, detour)?.enable()?;
        }
    }
    Ok(())
}

// --- direction-based buildings -----------------------------------------------------------

// rotate the entity's direction byte for the duration of one draw call,
// restore it exactly after. safe solo; paused entirely during Map::save
macro_rules! directional_draw {
    ($hooked:ident, $hook:ident, $dir_off:expr) => {
        fn $hooked(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
            // mark the draw phase (these run on prepare worker threads that
            // IN_RENDER_PREPARE doesn't cover)
            super::STATIC_DRAW_DEPTH.with(|d| d.set(d.get() + 1));
            let dir_off: Option<usize> = $dir_off;
            let q = super::rotation::dir_rot_steps();
            let mut restore: Option<(usize, u8)> = None;
            if q != 0 && !this.is_null() && !SAVE_ACTIVE.load(Ordering::SeqCst) {
                if let Some(off) = dir_off {
                    unsafe {
                        let ptr = (this as *const u8).add(off) as *mut u8;
                        let d = std::ptr::read(ptr);
                        // plausibility: a 16-direction byte. anything else
                        // means the offset shifted — degrade to no-op
                        if d < 16 {
                            std::ptr::write(ptr, (d + 4 * q) & 0x0F);
                            restore = Some((off, d));
                        }
                    }
                }
            }
            unsafe { $hook.call(this, queue) }
            if let Some((off, d)) = restore {
                unsafe { std::ptr::write((this as *const u8).add(off) as *mut u8, d) };
            }
            super::STATIC_DRAW_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    };
}

directional_draw!(hooked_splitter_draw_base, SplitterDrawBaseHook, Some(offsets::TBC_DIR_OFF));
directional_draw!(hooked_lane_splitter_draw_base, LaneSplitterDrawBaseHook, Some(offsets::TBC_DIR_OFF));
directional_draw!(hooked_ug_belt_draw_base, UgBeltDrawBaseHook, Some(offsets::TBC_DIR_OFF));
directional_draw!(hooked_inserter_draw, InserterDrawHook, Some(offsets::INSERTER_DIR_OFF));
directional_draw!(hooked_pipe_to_ground_draw, PipeToGroundDrawHook, Some(offsets::INSERTER_DIR_OFF));
// plain pipes rotate their connection mask in the Pipe::getSpriteGroup shim;
// this bracket only provides the draw-phase flag that opens that shim's gate
directional_draw!(hooked_pipe_draw, PipeDrawHook, None);

// thrusters lie on the platform floor — record their sprites as flat quads
fn hooked_thruster_draw(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
    super::IN_FLAT_DRAW.with(|f| f.set(true));
    unsafe { ThrusterDrawHook.call(this, queue) }
    super::IN_FLAT_DRAW.with(|f| f.set(false));
}

// solar panels + rocket silo: laid flat AND raised onto a low platform
macro_rules! flat_elevated_draw {
    ($hooked:ident, $hook:ident) => {
        fn $hooked(this: *mut core::ffi::c_void, queue: *mut core::ffi::c_void) {
            super::IN_FLAT_DRAW.with(|f| f.set(true));
            super::IN_FLAT_ELEVATED.with(|f| f.set(true));
            unsafe { $hook.call(this, queue) }
            super::IN_FLAT_ELEVATED.with(|f| f.set(false));
            super::IN_FLAT_DRAW.with(|f| f.set(false));
        }
    };
}
flat_elevated_draw!(hooked_solar_panel_draw, SolarPanelDrawHook);
flat_elevated_draw!(hooked_rocket_silo_draw, RocketSiloDrawHook);

fn hooked_map_save(
    this: *mut core::ffi::c_void,
    serialiser: *mut core::ffi::c_void,
    observer: *mut core::ffi::c_void,
) {
    SAVE_ACTIVE.store(true, Ordering::SeqCst);
    unsafe { MapSaveHook.call(this, serialiser, observer) }
    SAVE_ACTIVE.store(false, Ordering::SeqCst);
}

// direction-based draw hooks. entity draws are hot on the prepare path, so
// the detours are enabled with all other threads suspended
pub fn install_directional(symbols: &SymbolMap, base: usize) -> Result<()> {
    let targets: [(&GameFn, &'static EntityDrawDetour, fn(_, _)); 9] = [
        (&offsets::SPLITTER_DRAW_BASE, &SplitterDrawBaseHook, hooked_splitter_draw_base as fn(_, _)),
        (&offsets::LANE_SPLITTER_DRAW_BASE, &LaneSplitterDrawBaseHook, hooked_lane_splitter_draw_base),
        (&offsets::UG_BELT_DRAW_BASE, &UgBeltDrawBaseHook, hooked_ug_belt_draw_base),
        (&offsets::INSERTER_DRAW, &InserterDrawHook, hooked_inserter_draw),
        (&offsets::PIPE_TO_GROUND_DRAW, &PipeToGroundDrawHook, hooked_pipe_to_ground_draw),
        (&offsets::PIPE_DRAW, &PipeDrawHook, hooked_pipe_draw),
        (&offsets::THRUSTER_DRAW, &ThrusterDrawHook, hooked_thruster_draw),
        (&offsets::SOLAR_PANEL_DRAW, &SolarPanelDrawHook, hooked_solar_panel_draw),
        (&offsets::ROCKET_SILO_DRAW, &RocketSiloDrawHook, hooked_rocket_silo_draw),
    ];
    for (gf, hook, detour) in &targets {
        let addr = super::resolve(symbols, base, gf);
        unsafe {
            let target: FnEntityDraw = std::mem::transmute(addr);
            hook.initialize(target, *detour)?;
        }
    }
    let res: Result<()> = super::rotation::with_other_threads_suspended(|| unsafe {
        for (_, hook, _) in &targets {
            hook.enable()?;
        }
        Ok(())
    });
    res?;

    // Map::save is never called from draw paths — a plain detour is safe
    let addr = super::resolve(symbols, base, &offsets::MAP_SAVE);
    unsafe {
        MapSaveHook.initialize(std::mem::transmute(addr), hooked_map_save)?.enable()?;
    }
    Ok(())
}

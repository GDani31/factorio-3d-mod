// sprite-level hooks:
// - the DrawQueue methods every world sprite goes through (layer retagging
//   for belts/rails + camera-facing rotation of orientation arguments)
// - the placement helper (records each object sprite's exact screen rect)
// - drawEntities (diverts layer ranges into the capture targets)
// - the sprite-batch flush (force-flushed at capture window boundaries)

use crate::billboards::BbRect;
use crate::capture::{self, CaptureKind, HiKind};
use crate::offsets;
use crate::symbols::SymbolMap;
use anyhow::Result;
use retour::static_detour;
use std::sync::Mutex;

use super::MapPos;

// --- layer retagging ------------------------------------------------------------
// belt/rail sprites get retagged onto spare late layers while capture is on:
// 155 for belts/rails, 156 for items (so items stay above belts). the
// [155,159) drawEntities call then carries them into the lifted-plane capture.
// lower-object layers 118/119 are retagged into the captured object range so
// inserter bases etc. stand up too.

pub(super) fn retarget_layer(layer: u8) -> u8 {
    match layer {
        68 | 76 | 84 | 96 | 97 | 100 | 104 | 105 | 117 => 155,
        109 => 156,
        118 | 119 => 121,
        _ => layer,
    }
}

// layers that belong to belts/rails inside the ground ranges
fn is_belt_layer(l: u8) -> bool {
    matches!(l, 65 | 68 | 76 | 84 | 96 | 97 | 100 | 104 | 105 | 109 | 117)
}

// camera-facing rotation for an orientation ARGUMENT (no game state touched).
// only applies to billboard-bound sprites while capture is on
pub(super) fn rotated_arg_for(orientation: *const f32, layer: u8) -> (f32, bool) {
    if orientation.is_null() {
        return (0.0, false);
    }
    let billboard_bound = (118..133).contains(&layer)
        || super::ENTITY_DRAW_DEPTH.with(|d| d.get()) > 0
        || super::IN_CHARACTER_DRAW.with(|f| f.get());
    if !billboard_bound || !capture::capture_enabled() {
        return (0.0, false);
    }
    let (yaw, _p, _z) = crate::camera::get();
    if yaw.abs() <= 1.0 {
        return (0.0, false);
    }
    let o = unsafe { *orientation };
    if !o.is_finite() {
        return (0.0, false);
    }
    (
        (o + (crate::settings::CHAR_ROT_SIGN as f32) * yaw / 360.0).rem_euclid(1.0),
        true,
    )
}

// --- function types ----------------------------------------------------------------
// abi notes (2.0.77 disassembly): layer is a byte enum on the stack;
// orientation (RealOrientation) is an f32 passed BY REFERENCE

type FnDqDraw = unsafe extern "C" fn(
    *mut core::ffi::c_void,      // this
    *const core::ffi::c_void,    // sprite
    *const MapPos,               // position
    u32,                         // DrawingFlags
    u8,                          // RenderLayer
    *const core::ffi::c_void,    // Vector / pair<Vector,Vector>
    i8,                          // sort sub-order
    f32,
);
// place(params, Sprite*, RenderParameters*, MapPosition*, Vector* out)
type FnPlaceSprite = unsafe extern "C" fn(
    *mut core::ffi::c_void,
    *const core::ffi::c_void,
    *const core::ffi::c_void,
    *const MapPos,
    *mut f64,
) -> *mut core::ffi::c_void;
type FnDrawEntities =
    unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, u8, u8) -> u32;

static_detour! {
    static DrawVecHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, u32, u8, *const core::ffi::c_void, i8, f32);
    static DrawPairHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, u32, u8, *const core::ffi::c_void, i8, f32);
    // drawTinted — Color (16 bytes) goes by hidden pointer in r9
    static DrawTintedHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, *const core::ffi::c_void, u32, u8, *const core::ffi::c_void, i8, f32);
    static DrawRotatedHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, *const f32, u32, u8, *const core::ffi::c_void, i8);
    static DrawRotatedWithoutTintHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, *const f32, u32, u8, *const core::ffi::c_void, i8);
    static DrawRotatedTintedHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, *const f32, *const core::ffi::c_void, u32, u8, *const core::ffi::c_void, i8);
    static DrawShiftedRotHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, *const f32, f64, f64, *const core::ffi::c_void, u32, u8, i8);
    // the train path: carries a precomputed SincosResult
    static DrawShiftedRotPreciseHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const core::ffi::c_void, *const f32, *const f64, f64, f64, u32, u8);
    static PlaceSpriteHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const core::ffi::c_void, *const MapPos, *mut f64) -> *mut core::ffi::c_void;
    static DrawEntitiesHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, u8, u8) -> u32;
    static DcbFlushHook: unsafe extern "C" fn(*mut core::ffi::c_void, u8);
}

pub fn install(symbols: &SymbolMap, base: usize) -> Result<()> {
    unsafe {
        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_VEC);
        let t: FnDqDraw = std::mem::transmute(addr);
        DrawVecHook.initialize(t, hooked_draw_vec)?.enable()?;

        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_PAIR);
        let t: FnDqDraw = std::mem::transmute(addr);
        DrawPairHook.initialize(t, hooked_draw_pair)?.enable()?;

        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_TINTED);
        DrawTintedHook.initialize(std::mem::transmute(addr), hooked_draw_tinted)?.enable()?;

        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_ROTATED);
        DrawRotatedHook.initialize(std::mem::transmute(addr), hooked_draw_rotated)?.enable()?;

        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_ROTATED_WITHOUT_TINT);
        DrawRotatedWithoutTintHook
            .initialize(std::mem::transmute(addr), hooked_draw_rotated_without_tint)?
            .enable()?;

        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_ROTATED_TINTED);
        DrawRotatedTintedHook
            .initialize(std::mem::transmute(addr), hooked_draw_rotated_tinted)?
            .enable()?;

        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_SHIFTED_ROT);
        DrawShiftedRotHook
            .initialize(std::mem::transmute(addr), hooked_draw_shifted_rot)?
            .enable()?;

        let addr = super::resolve(symbols, base, &offsets::DQ_DRAW_SHIFTED_ROT_PRECISE);
        DrawShiftedRotPreciseHook
            .initialize(std::mem::transmute(addr), hooked_draw_shifted_rot_precise)?
            .enable()?;

        let addr = super::resolve(symbols, base, &offsets::PLACE_SPRITE);
        let t: FnPlaceSprite = std::mem::transmute(addr);
        PlaceSpriteHook.initialize(t, hooked_place_sprite)?.enable()?;

        let addr = super::resolve(symbols, base, &offsets::DRAW_ENTITIES);
        let t: FnDrawEntities = std::mem::transmute(addr);
        DrawEntitiesHook.initialize(t, hooked_draw_entities)?.enable()?;

        let addr = super::resolve(symbols, base, &offsets::DCB_FLUSH);
        let t: unsafe extern "C" fn(*mut core::ffi::c_void, u8) = std::mem::transmute(addr);
        DcbFlushHook.initialize(t, hooked_dcb_flush)?.enable()?;
    }
    Ok(())
}

// --- DrawQueue hooks -----------------------------------------------------------
// each one: maybe rotate the orientation argument, maybe retag the layer,
// remember the layer in a thread-local for the placement hook, pass through

fn retag(layer: u8) -> u8 {
    if capture::capture_enabled() { retarget_layer(layer) } else { layer }
}

fn with_layer(layer: u8, f: impl FnOnce()) {
    super::CURRENT_DRAW_LAYER.with(|c| c.set(layer));
    f();
    super::CURRENT_DRAW_LAYER.with(|c| c.set(255));
}

fn hooked_draw_vec(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    flags: u32,
    layer: u8,
    vec: *const core::ffi::c_void,
    sub: i8,
    f: f32,
) {
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawVecHook.call(this, sprite, pos, flags, layer, vec, sub, f)
    });
}

fn hooked_draw_pair(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    flags: u32,
    layer: u8,
    vec: *const core::ffi::c_void,
    sub: i8,
    f: f32,
) {
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawPairHook.call(this, sprite, pos, flags, layer, vec, sub, f)
    });
}

fn hooked_draw_tinted(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    color: *const core::ffi::c_void,
    flags: u32,
    layer: u8,
    vec: *const core::ffi::c_void,
    sub: i8,
    f: f32,
) {
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawTintedHook.call(this, sprite, pos, color, flags, layer, vec, sub, f)
    });
}

fn hooked_draw_rotated(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    orientation: *const f32,
    flags: u32,
    layer: u8,
    vec: *const core::ffi::c_void,
    sub: i8,
) {
    let (rot, use_rot) = rotated_arg_for(orientation, layer);
    let ori = if use_rot { &rot as *const f32 } else { orientation };
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawRotatedHook.call(this, sprite, pos, ori, flags, layer, vec, sub)
    });
}

fn hooked_draw_rotated_without_tint(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    orientation: *const f32,
    flags: u32,
    layer: u8,
    vec: *const core::ffi::c_void,
    sub: i8,
) {
    let (rot, use_rot) = rotated_arg_for(orientation, layer);
    let ori = if use_rot { &rot as *const f32 } else { orientation };
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawRotatedWithoutTintHook.call(this, sprite, pos, ori, flags, layer, vec, sub)
    });
}

fn hooked_draw_rotated_tinted(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    orientation: *const f32,
    color: *const core::ffi::c_void,
    flags: u32,
    layer: u8,
    vec: *const core::ffi::c_void,
    sub: i8,
) {
    let (rot, use_rot) = rotated_arg_for(orientation, layer);
    let ori = if use_rot { &rot as *const f32 } else { orientation };
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawRotatedTintedHook.call(this, sprite, pos, ori, color, flags, layer, vec, sub)
    });
}

fn hooked_draw_shifted_rot(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    orientation: *const f32,
    sx: f64,
    sy: f64,
    color: *const core::ffi::c_void,
    flags: u32,
    layer: u8,
    sub: i8,
) {
    // the shift placement must rotate in step with the rotated frame pick
    let (rot, use_rot) = rotated_arg_for(orientation, layer);
    let ori = if use_rot { &rot as *const f32 } else { orientation };
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawShiftedRotHook.call(this, sprite, pos, ori, sx, sy, color, flags, layer, sub)
    });
}

fn hooked_draw_shifted_rot_precise(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    vec: *const core::ffi::c_void,
    orientation: *const f32,
    sincos: *const f64,
    n1: f64,
    n2: f64,
    flags: u32,
    layer: u8,
) {
    // trains: the wagon's sub-parts are placed along the wagon axis via this
    // orientation + sin/cos pair. rotate BOTH by the same yaw or the parts
    // spread along the old axis while showing the new frame
    let (rot, use_rot) = rotated_arg_for(orientation, layer);
    let mut new_sincos = [0.0f64; 2];
    let (ori, sc) = if use_rot && !sincos.is_null() {
        let (s, c) = unsafe { (*sincos, *sincos.add(1)) };
        // only touch a real unit sin/cos — a wrong layout degrades to no-op
        if (s * s + c * c - 1.0).abs() < 0.01 {
            let delta =
                ((rot - unsafe { *orientation }) as f64).rem_euclid(1.0) * std::f64::consts::TAU;
            let (sd, cd) = delta.sin_cos();
            new_sincos[0] = s * cd + c * sd;
            new_sincos[1] = c * cd - s * sd;
            (&rot as *const f32, new_sincos.as_ptr())
        } else {
            (orientation, sincos)
        }
    } else {
        (orientation, sincos)
    };
    let layer = retag(layer);
    with_layer(layer, || unsafe {
        DrawShiftedRotPreciseHook.call(this, sprite, vec, ori, sc, n1, n2, flags, layer)
    });
}

// inserter arms and similar rotated sprites. inside a static entity draw
// they render flat (the arm sweeps in a horizontal plane; standing it up
// looked wrong) with a vanilla orientation — the geometric ground rotation
// replaces the frame rotation there
pub(super) fn hooked_draw_scaled_rotated(
    this: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    pos: *const MapPos,
    orientation: *const f32,
    sx: f64,
    sy: f64,
    flags: u32,
    layer: u8,
    vec: *const core::ffi::c_void,
    sub: i8,
) {
    let in_static = super::STATIC_DRAW_DEPTH.with(|d| d.get()) > 0;
    let (rot, use_rot) =
        if in_static { (0.0, false) } else { rotated_arg_for(orientation, layer) };
    let ori = if use_rot { &rot as *const f32 } else { orientation };
    let layer = retag(layer);
    super::CURRENT_DRAW_LAYER.with(|c| c.set(layer));
    if in_static {
        super::IN_FLAT_DRAW.with(|f| f.set(true));
    }
    unsafe {
        super::rotation::DrawScaledRotatedHook
            .call(this, sprite, pos, ori, sx, sy, flags, layer, vec, sub)
    };
    if in_static {
        super::IN_FLAT_DRAW.with(|f| f.set(false));
    }
    super::CURRENT_DRAW_LAYER.with(|c| c.set(255));
}

// --- placement hook: record billboard rects --------------------------------

fn hooked_place_sprite(
    params: *mut core::ffi::c_void,
    sprite: *const core::ffi::c_void,
    rp: *const core::ffi::c_void,
    pos: *const MapPos,
    out_center: *mut f64,
) -> *mut core::ffi::c_void {
    let ret = unsafe { PlaceSpriteHook.call(params, sprite, rp, pos, out_center) };

    let layer = super::CURRENT_DRAW_LAYER.with(|c| c.get());
    let is_player = super::IN_CHARACTER_DRAW.with(|f| f.get());
    // vehicles/trains never set a layer (they draw through specialized
    // paths); their draw hooks bracket them instead
    let is_vehicle = super::ENTITY_DRAW_DEPTH.with(|d| d.get()) > 0;
    // wires (131) span BETWEEN poles — they get their own plane, not billboards
    let is_object = (120..133).contains(&layer) && layer != 131;

    if !(is_object || is_player || is_vehicle)
        || params.is_null()
        || !capture::capture_enabled()
    {
        return ret;
    }
    // shadow/light sprites render into their own framebuffers — their pixels
    // aren't in the object capture, so a billboard would show a ghost copy
    let is_shadow_or_light = !sprite.is_null()
        && unsafe { *((sprite as *const u8).add(offsets::SPRITE_FLAGS) as *const u16) }
            & offsets::SPRITE_SHADOW_OR_LIGHT_BITS
            != 0;
    if is_shadow_or_light {
        return ret;
    }
    // only main-view sprites: the game draws all entities a second time each
    // frame in a different pass — recording those misplaced every billboard
    if rp.is_null() || !unsafe { super::frame::rp_matches_main_view(rp) } {
        return ret;
    }

    unsafe {
        let pf = |off: usize| *((params as *const u8).add(off) as *const f32);
        let cx = pf(offsets::PARAM_CENTER_X);
        let cy = pf(offsets::PARAM_CENTER_Y);
        let hw = 0.5 * pf(offsets::PARAM_SIZE_X) * pf(offsets::PARAM_SCALE_X);
        let hh = 0.5 * pf(offsets::PARAM_SIZE_Y) * pf(offsets::PARAM_SCALE_Y);
        // all-zero tint = invisible sprite; its quad would sample neighbors
        let tint = *((params as *const u8).add(offsets::PARAM_TINT) as *const u32);

        let (rpw, rph) = (
            *((rp as *const u8).add(offsets::RP_WIDTH) as *const u16) as f32,
            *((rp as *const u8).add(offsets::RP_HEIGHT) as *const u16) as f32,
        );

        // player/vehicle: the flashlight/headlight cone is emitted through the
        // same draw but renders into the light framebuffer — skip huge parts
        let size_sane = !(is_player || is_vehicle) || (hw < 0.5 * rpw && hh < 0.5 * rph);
        // skip the wide-flat shadow part (players: body parts are all taller
        // than 0.6*width; vehicles are legitimately wider, use 0.35)
        let skip_shadow = if is_player {
            hh > 0.6 * hw
        } else if is_vehicle {
            hh > 0.35 * hw
        } else {
            true
        };
        // second-pass discriminator: a real pass satisfies
        // width_px = span_tiles * scale * 32; the copied-rect pass is off by
        // the boost factor (its pixels are vanilla-scale)
        let rp_consistent = {
            let rect = (rp as *const u8).add(offsets::RP_RECT) as *const i32;
            let span_x = (*rect.add(2) - *rect.add(0)) as f32 / offsets::RECT_FP;
            let scale = *((rp as *const u8).add(offsets::RP_SCALE) as *const f64) as f32;
            let expected_w = span_x * scale * 32.0;
            !(expected_w > 1.0) || (rpw > 1.0 && (0.85..=1.18).contains(&(rpw / expected_w)))
        };

        if rp_consistent
            && hw > 0.05
            && hh > 0.05
            && tint != 0
            && hw < rpw * 4.0
            && hh < rph * 4.0
            && size_sane
            && cx + hw > 0.0
            && cx - hw < rpw
            && cy + hh > 0.0
            && cy - hh < rph
            && skip_shadow
        {
            // stamp = THIS rp's own view rect (several RenderParameters
            // exist per frame; mixing rects floats sprites into the sky)
            let mut own_stamp = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            {
                let rect = (rp as *const u8).add(offsets::RP_RECT) as *const i32;
                let left_t = *rect.add(0) as f32 / offsets::RECT_FP;
                let top_t = *rect.add(1) as f32 / offsets::RECT_FP;
                let span_x = *rect.add(2) as f32 / offsets::RECT_FP - left_t;
                let span_y = *rect.add(3) as f32 / offsets::RECT_FP - top_t;
                if span_x > 0.1 && span_y > 0.1 {
                    own_stamp = (left_t, top_t, span_x, span_y);
                }
            }

            let (kx, ky) = if pos.is_null() { (cx as i32, cy as i32) } else { ((*pos).x, (*pos).y) };
            let grp = if is_player || is_vehicle {
                super::ENTITY_DRAW_SERIAL.with(|c| c.get())
            } else {
                0
            };
            crate::billboards::record(BbRect {
                cx,
                cy,
                hw,
                hh,
                kx,
                ky,
                player: is_player,
                unit: super::IN_UNIT_DRAW.with(|d| d.get()) > 0,
                flat: super::IN_FLAT_DRAW.with(|f| f.get()),
                special: is_player || is_vehicle,
                grp,
                stamp: own_stamp,
            });
        }
    }
    ret
}

// --- drawEntities: divert layer ranges into captures ----------------------------

fn hooked_draw_entities(
    this: *mut core::ffi::c_void,
    queues: *const core::ffi::c_void,
    from: u8,
    to: u8,
) -> u32 {
    if capture::capture_enabled() && capture::world_render_active() {
        // object / belt / elevated ranges -> their capture targets
        let kind = if (120..133).contains(&from) && to <= 133 {
            Some(CaptureKind::Object)
        } else if (155..159).contains(&from) && to <= 159 {
            Some(CaptureKind::Belt)
        } else if (133..153).contains(&from) && to <= 153 {
            Some(CaptureKind::Elevated)
        } else {
            None
        };
        if let Some(kind) = kind {
            return divert_object_range(this, queues, from, to, kind);
        } else if from < 120 {
            return divert_ground_range(this, queues, from, to);
        }
    }
    unsafe { DrawEntitiesHook.call(this, queues, from, to) }
}

// object-range handling: split out the wire layer, divert each segment,
// then replay the object segments into the hi-res tiles
fn divert_object_range(
    this: *mut core::ffi::c_void,
    queues: *const core::ffi::c_void,
    from: u8,
    to: u8,
    kind: CaptureKind,
) -> u32 {
    // wires (131) get their own capture: as billboards they'd be floating
    // mega-quads; as a lifted plane they rotate correctly
    let mut segs: Vec<(u8, u8, CaptureKind)> = Vec::new();
    if kind == CaptureKind::Object && from <= 131 && to > 131 {
        if from < 131 {
            segs.push((from, 131, CaptureKind::Object));
        }
        segs.push((131, 132, CaptureKind::Wire));
        if to > 132 {
            segs.push((132, to, CaptureKind::Object));
        }
    } else {
        segs.push((from, to, kind));
    }

    let mut r = 0u32;
    let mut any_captured = false;
    for (f, t, k) in &segs {
        // pending batched content belongs to the PREVIOUS target — flush it
        // there before the window opens (it used to smear into the capture)
        force_flush_batches();
        if capture::begin_capture(*k) {
            r += unsafe { DrawEntitiesHook.call(this, queues, *f, *t) };
            // flush the window's own content while our target is still bound
            force_flush_batches();
            capture::end_capture();
            any_captured = true;
        } else {
            r += unsafe { DrawEntitiesHook.call(this, queues, *f, *t) };
        }
    }
    if !any_captured || kind != CaptureKind::Object {
        return r;
    }

    // hi-res replay: the same object segments once more, into this frame's
    // rotation tile (one window per call — more windows starve the tiles),
    // plus the every-frame live tile for moving sprites
    let b = super::frame::zoom_boost_applied();
    if b > 1.05 {
        let grid = capture::hi_grid_for(b);
        for live in [false, true] {
            if capture::begin_hi_capture(HiKind::Object, b, grid, live) {
                for (f, t, k) in &segs {
                    if *k == CaptureKind::Object {
                        unsafe { DrawEntitiesHook.call(this, queues, *f, *t) };
                    }
                }
                force_flush_batches();
                capture::end_hi_capture();
            }
        }
    }
    r
}

// ground ranges (below layer 120): belt/rail layers divert into the belt
// capture (-> lifted plane), the rest renders normally and replays into the
// ground hi tile
fn divert_ground_range(
    this: *mut core::ffi::c_void,
    queues: *const core::ffi::c_void,
    from: u8,
    to: u8,
) -> u32 {
    // split the range into belt / non-belt segments
    let mut segs: Vec<(u8, u8, bool)> = Vec::new();
    let mut s = from;
    let mut cur = is_belt_layer(from);
    for l in from + 1..to {
        let bl = is_belt_layer(l);
        if bl != cur {
            segs.push((s, l, cur));
            s = l;
            cur = bl;
        }
    }
    segs.push((s, to, cur));

    let mut total = 0u32;
    for (f, t, belt) in &segs {
        if *belt {
            force_flush_batches();
            if capture::begin_capture(CaptureKind::Belt) {
                total += unsafe { DrawEntitiesHook.call(this, queues, *f, *t) };
                force_flush_batches();
                capture::end_capture();
                continue;
            }
        }
        total += unsafe { DrawEntitiesHook.call(this, queues, *f, *t) };
    }

    let b = super::frame::zoom_boost_applied();
    if b > 1.05 {
        force_flush_batches();
        if capture::begin_hi_capture(HiKind::Ground, b, capture::hi_grid_for(b), false) {
            for (f, t, belt) in &segs {
                if !*belt {
                    unsafe { DrawEntitiesHook.call(this, queues, *f, *t) };
                }
            }
            force_flush_batches();
            capture::end_hi_capture();
        }
    }
    total
}

// --- sprite-batch flush --------------------------------------------------------
// the game issues its gpu draws lazily. to capture deterministically, every
// live DrawCommandBatch is force-flushed at capture window boundaries

// live batch instances: (this, last bool arg, last-seen frame)
static DCB_REGISTRY: Mutex<Vec<(usize, u8, u64)>> = Mutex::new(Vec::new());

fn hooked_dcb_flush(this: *mut core::ffi::c_void, hard: u8) {
    if !this.is_null() {
        let frame = super::frame::frame_count();
        if let Ok(mut reg) = DCB_REGISTRY.lock() {
            match reg.iter_mut().find(|(p, _, _)| *p == this as usize) {
                Some(e) => {
                    e.1 = hard;
                    e.2 = frame;
                }
                None => {
                    if reg.len() < 8 {
                        reg.push((this as usize, hard, frame));
                    }
                }
            }
        }
    }
    unsafe { DcbFlushHook.call(this, hard) }
}

// force-flush every recently-seen batch (stale entries are skipped — level
// loads destroy batch objects, and a dangling call would crash)
pub fn force_flush_batches() {
    let frame = super::frame::frame_count();
    let entries: Vec<(usize, u8)> = match DCB_REGISTRY.lock() {
        Ok(reg) => reg
            .iter()
            .filter(|(_, _, seen)| frame.saturating_sub(*seen) <= 120)
            .map(|(p, a, _)| (*p, *a))
            .collect(),
        Err(_) => return,
    };
    for (ptr, arg) in entries {
        unsafe { DcbFlushHook.call(ptr as *mut core::ffi::c_void, arg) };
    }
}

// called on level-load events so no destroyed batch is ever flushed
pub fn clear_batch_registry() {
    if let Ok(mut reg) = DCB_REGISTRY.lock() {
        reg.clear();
    }
}

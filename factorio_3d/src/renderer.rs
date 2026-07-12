// per-frame orchestration: right after the game finishes rendering the
// world fbo, warp it in place. factorio then composites the warped fbo and
// draws the hud flat on top — hud separation is automatic.

use crate::billboards::BbRect;
use crate::camera;
use crate::capture::{self, LIVE_TILE, MAX_HI_SLICES};
use crate::settings;
use crate::warp::{BillboardUv, WarpLayers, WarpParams, WarpPipeline};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::core::Interface;

static WARP_ENGAGED_LOGGED: AtomicBool = AtomicBool::new(false);

struct DxState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    warp: WarpPipeline,
}

pub struct Renderer3D {
    frame_count: AtomicU64,
    dx_state: Mutex<Option<DxState>>,
    dx_initialized: AtomicBool,
}

impl Renderer3D {
    pub fn new() -> Result<Self> {
        Ok(Self {
            frame_count: AtomicU64::new(0),
            dx_state: Mutex::new(None),
            dx_initialized: AtomicBool::new(false),
        })
    }

    // called from the dxgi Present hook: initializes d3d state, polls input
    pub fn on_present(&self, swapchain_ptr: *mut core::ffi::c_void) {
        let frame = self.frame_count.fetch_add(1, Ordering::Relaxed);
        camera::poll();

        if !self.dx_initialized.load(Ordering::Relaxed) {
            let swapchain: IDXGISwapChain = unsafe {
                match IDXGISwapChain::from_raw_borrowed(&swapchain_ptr) {
                    Some(sc) => sc.clone(),
                    None => return,
                }
            };
            match self.init_from_swapchain(&swapchain) {
                Ok(()) => {
                    self.dx_initialized.store(true, Ordering::Relaxed);
                    log::info!("[renderer] d3d11 initialized");
                }
                Err(e) => {
                    if frame < 5 {
                        log::error!("[renderer] init failed: {e:#}");
                    }
                }
            }
        }
    }

    fn init_from_swapchain(&self, swapchain: &IDXGISwapChain) -> Result<()> {
        let device: ID3D11Device = unsafe { swapchain.GetDevice()? };
        let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext()? };
        let back_buffer: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0)? };

        // the real game window handle, for the input subclass
        let sc_desc = unsafe { swapchain.GetDesc()? };
        camera::set_game_hwnd(sc_desc.OutputWindow.0 as isize);

        let mut bb_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { back_buffer.GetDesc(&mut bb_desc) };
        crate::hooks::frame::set_base_size(bb_desc.Width, bb_desc.Height);

        let warp = WarpPipeline::new(&device, &back_buffer)?;
        capture::init(device.clone(), context.clone());

        *self.dx_state.lock().unwrap() = Some(DxState { device, context, warp });
        Ok(())
    }

    // called right after GameRenderer::render — the world fbo is still bound
    pub fn on_after_world_render(&self) {
        if !self.dx_initialized.load(Ordering::Relaxed) {
            return;
        }
        let (yaw, pitch, zoom) = camera::get();

        let mut state_guard = self.dx_state.lock().unwrap();
        let state = match state_guard.as_mut() {
            Some(s) => s,
            None => return,
        };
        let DxState { device, context, warp } = state;

        unsafe {
            // the currently bound render target is the world fbo
            let mut rtvs = [None; 1];
            context.OMGetRenderTargets(Some(&mut rtvs), None);
            let rtv = match &rtvs[0] {
                Some(r) => r,
                None => return,
            };
            let resource = match rtv.GetResource() {
                Ok(r) => r,
                Err(_) => return,
            };
            let world_tex = match resource.cast::<ID3D11Texture2D>() {
                Ok(t) => t,
                Err(_) => return,
            };
            let mut fbo_desc = D3D11_TEXTURE2D_DESC::default();
            world_tex.GetDesc(&mut fbo_desc);

            let plane_scale = crate::hooks::frame::zoom_boost_applied().max(1.0);
            let rotated =
                yaw.abs() > 1.0 || (pitch - 90.0).abs() > 1.0 || (zoom - 1.0).abs() > 0.05;

            // harvest this frame's captures; arm capture for the next frame
            // only while the warp runs (it's what composites the layers back)
            let cap = capture::take_frame_capture();
            capture::set_capture_enabled(rotated);

            if rotated && !WARP_ENGAGED_LOGGED.swap(true, Ordering::Relaxed) {
                log::info!("[renderer] warp engaged (yaw={yaw:.1} pitch={pitch:.1} zoom={zoom:.2})");
            }

            if !rotated {
                // camera returned to vanilla this very frame but layers were
                // already diverted: composite them back flat
                if let Some(c) = &cap {
                    for srv in
                        [c.belt.as_ref(), c.elevated.as_ref(), c.wire.as_ref(), c.object.as_ref()]
                            .into_iter()
                            .flatten()
                    {
                        warp.composite_flat(context, srv, fbo_desc.Width, fbo_desc.Height);
                    }
                }
                crate::picking::clear();
                return;
            }

            // captured layers only match the fbo when the sizes agree (a
            // mid-resize frame can briefly disagree)
            let matching =
                cap.as_ref().filter(|c| c.w == fbo_desc.Width && c.h == fbo_desc.Height);
            let layers = WarpLayers {
                object: matching.and_then(|c| c.object.as_ref()),
                object_hi: matching.and_then(|c| c.object_hi.as_ref()),
                ground_hi: matching.and_then(|c| c.ground_hi.as_ref()),
                belt: matching.and_then(|c| c.belt.as_ref()),
                elevated: matching.and_then(|c| c.elevated.as_ref()),
                wire: matching.and_then(|c| c.wire.as_ref()),
            };

            // per-tile affine: current view uv -> the view each hi tile was
            // rendered under (exact for any pan/zoom, so tile age is fine)
            let hi_grid = cap.as_ref().map(|c| c.hi_grid).unwrap_or(1).max(1);
            let (cl, ct, csx, csy) = crate::hooks::frame::view_rect_tiles();
            let mut tile_affine = [[0.0f32; 4]; MAX_HI_SLICES];
            let mut tile_cover = [0.0f32; MAX_HI_SLICES];
            if let Some(c) = cap.as_ref() {
                for i in 0..MAX_HI_SLICES {
                    let m = c.hi_meta[i];
                    let (sl, st, ssx, ssy) = m.stamp;
                    if m.cover > 0.01 && ssx > 0.1 && ssy > 0.1 && csx > 0.1 && csy > 0.1 {
                        let ax = csx / ssx;
                        let bx = (cl - sl) / ssx;
                        let ay = csy / ssy;
                        let by = (ct - st) / ssy;
                        // drop tiles from a teleport-scale different view
                        if (0.5..=2.0).contains(&ax)
                            && (0.5..=2.0).contains(&ay)
                            && bx.abs() < 0.5
                            && by.abs() < 0.5
                        {
                            tile_affine[i] = [ax, bx, ay, by];
                            tile_cover[i] = m.cover;
                        }
                    }
                }
            }
            let hi_ok = matching.map(|c| c.object_hi.is_some()).unwrap_or(false)
                && tile_cover.iter().any(|&cv| cv > 0.01);

            // build the billboard quads from the stored rect batches
            let mut billboards = Vec::new();
            if layers.object.is_some() {
                billboards = build_billboards(
                    fbo_desc.Width as f32,
                    fbo_desc.Height as f32,
                    hi_grid,
                    hi_ok,
                    &tile_cover,
                    yaw,
                    plane_scale,
                );
            }

            // lift heights: plane-z spans the render's world height in tiles
            let (_, _, _, span_y) = crate::hooks::frame::view_rect_tiles();
            let lift = |tiles: f32| {
                if span_y > 0.5 { tiles * 2.0 * plane_scale / span_y } else { 0.0 }
            };
            let params = WarpParams {
                yaw,
                pitch,
                zoom,
                plane_scale,
                belt_lift: lift(settings::BELT_LIFT_TILES),
                elevated_lift: lift(settings::ELEVATED_LIFT_TILES),
                elevated_south: lift(settings::ELEVATED_SOUTH_TILES),
                wire_lift: lift(settings::WIRE_LIFT_TILES),
                fps_eye_h: if span_y > 0.5 { 1.7 * 2.0 * plane_scale / span_y } else { 0.05 },
                hi_grid: hi_grid as f32,
                tile_affine: &tile_affine,
                tile_cover: &tile_cover,
            };
            warp.warp_in_place(device, context, rtv, &world_tex, &layers, &params, &billboards);
        }
    }
}

// union rect of one mobile entity's parts (drawn as ONE quad — per-part
// quads each showed the whole composited entity at their own offset)
struct SpecialGroup {
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    kx0: i32,
    kx1: i32,
    ky0: i32,
    ky1: i32,
    player: bool,
    unit: bool,
    flat: bool,
    fly: bool,
}

// per-entity feet for static entities: the lowest screen-bottom among the
// entity's parts, plus its world-extent bounds
struct FeetGroup {
    vb: f32,
    uc: f32,
    kx0: i32,
    kx1: i32,
    ky0: i32,
    ky1: i32,
}

// turn the stored rect batches into billboard quads, sorted far-to-near
fn build_billboards(
    fw: f32,
    fh: f32,
    hi_grid: u32,
    hi_ok: bool,
    tile_cover: &[f32; MAX_HI_SLICES],
    yaw: f32,
    plane_scale: f32,
) -> Vec<BillboardUv> {
    let batches = crate::billboards::take_batches();
    let (cl, ct, csx, csy) = crate::hooks::frame::view_rect_tiles();
    // tiles -> plane-unit height for this frame's view
    let to_plane = |tiles: f32| if csy > 0.5 { tiles * 2.0 * plane_scale / csy } else { 0.0 };
    // flying-robot lift (plane units) + south shift (texture v)
    let fly_lift = to_plane(settings::BOT_LIFT_TILES);
    let bot_south_v = if csy > 0.5 { settings::BOT_SOUTH_TILES / csy } else { 0.0 };
    // solar panel / rocket silo platform height (plane units)
    let platform_lift = to_plane(settings::FLAT_PLATFORM_TILES);
    let total: usize = batches.iter().map(|b| b.rects.len()).sum();
    let mut billboards: Vec<BillboardUv> = Vec::with_capacity(total);

    for batch in &batches {
        // affine from the batch's stamp into the current view
        let (sl, st, ssx, ssy) = batch.stamp;
        if !(ssx > 0.1 && ssy > 0.1 && csx > 0.1 && csy > 0.1) {
            continue;
        }
        let au = ssx / csx;
        let bu = (sl - cl) / csx;
        let av = ssy / csy;
        let bv = (st - ct) / csy;
        // drop wild transforms (batches from a very different view)
        if !(0.5..=2.0).contains(&au) || !(0.5..=2.0).contains(&av) || bu.abs() > 0.75 || bv.abs() > 0.75
        {
            continue;
        }

        // split off the mobile entities and union their parts per serial.
        // specials re-record every frame, so their stamp must be at most one
        // frame from the current view — anything farther is poison
        let specials_ok = (0.8..=1.25).contains(&au)
            && (0.8..=1.25).contains(&av)
            && bu.abs() < 0.2
            && bv.abs() < 0.2;
        let mut normals: Vec<BbRect> = Vec::with_capacity(batch.rects.len());
        let mut sgroups: HashMap<u32, SpecialGroup> = HashMap::new();
        for r in &batch.rects {
            if !r.special || r.grp == 0 {
                normals.push(*r);
                continue;
            }
            if !specials_ok {
                continue;
            }
            let e = sgroups.entry(r.grp).or_insert(SpecialGroup {
                x0: r.cx - r.hw,
                x1: r.cx + r.hw,
                y0: r.cy - r.hh,
                y1: r.cy + r.hh,
                kx0: r.kx,
                kx1: r.kx,
                ky0: r.ky,
                ky1: r.ky,
                player: r.player,
                unit: r.unit,
                flat: r.flat,
                fly: r.fly,
            });
            e.x0 = e.x0.min(r.cx - r.hw);
            e.x1 = e.x1.max(r.cx + r.hw);
            e.y0 = e.y0.min(r.cy - r.hh);
            e.y1 = e.y1.max(r.cy + r.hh);
            e.kx0 = e.kx0.min(r.kx);
            e.kx1 = e.kx1.max(r.kx);
            e.ky0 = e.ky0.min(r.ky);
            e.ky1 = e.ky1.max(r.ky);
            e.player |= r.player;
            e.unit |= r.unit;
            e.flat |= r.flat;
            e.fly |= r.fly;
        }

        // one quad per mobile entity
        for g in sgroups.values() {
            // canvas guard: a real player/vehicle/bot is small. if the union
            // spans most of the frame, a spurious sprite from the game's second
            // (vanilla-scale) entity pass got merged in by serial — drawing it
            // would paste the whole frame as the entity ("player replaced with
            // the canvas"). drop the group instead.
            if (g.x1 - g.x0) / fw > 0.6 || (g.y1 - g.y0) / fh > 0.6 {
                continue;
            }
            let u0 = (g.x0 / fw) * au + bu;
            let u1 = (g.x1 / fw) * au + bu;
            let v_top = (g.y0 / fh) * av + bv;
            let v_base = (g.y1 / fh) * av + bv;
            if u1 <= 0.0 || u0 >= 1.0 || v_base <= 0.02 || v_top >= 1.0 {
                continue;
            }
            if g.player && camera::fps_mode() {
                continue; // first person: don't render yourself
            }
            // entity world center (kx/ky are absolute 1/256-tile fixed point)
            let kxc = 0.5 * (g.kx0 as f32 + g.kx1 as f32) / 256.0;
            let kyc = 0.5 * (g.ky0 as f32 + g.ky1 as f32) / 256.0;
            if g.flat {
                // trains: vanilla content laid flat at its capture position —
                // the ground rotation keeps the wagon exactly on its rails
                billboards.push(BillboardUv {
                    u0,
                    u1,
                    v_top,
                    v_base,
                    v_foot: v_base.clamp(0.0, 1.0),
                    sel: -1.0,
                    pu: 0.5 * (u0 + u1),
                    flat: true,
                    fly_lift: 0.0,
                });
                continue;
            }
            let (pu, v_foot) = if g.fly {
                // flying robots: screen-bottom anchor shifted south
                (0.5 * (u0 + u1), (v_base + bot_south_v).clamp(0.0, 1.0))
            } else if g.player || g.unit {
                // player + enemies: screen-bottom anchor (compact bodies)
                (0.5 * (u0 + u1), v_base.clamp(0.0, 1.0))
            } else if csx > 0.1 && csy > 0.1 {
                // vehicles: world map anchor + ~1.5 tiles to the visual bottom
                ((kxc - cl) / csx, ((kyc - ct) / csy + 1.5 / csy).clamp(0.0, 1.0))
            } else {
                (0.5 * (u0 + u1), v_base.clamp(0.0, 1.0))
            };
            if v_foot <= 0.02 || pu <= 0.0 || pu >= 1.0 {
                continue;
            }
            // moving sprites only ever sample the every-frame live tile
            let sel = if !hi_ok {
                -1.0
            } else {
                let cov = tile_cover[LIVE_TILE];
                if cov > 0.01 && (pu - 0.5).abs() < 0.5 * cov && (v_foot - 0.5).abs() < 0.5 * cov {
                    LIVE_TILE as f32
                } else {
                    -1.0
                }
            };
            billboards.push(BillboardUv {
                u0,
                u1,
                v_top,
                v_base,
                v_foot,
                sel,
                pu,
                flat: false,
                fly_lift: if g.fly { fly_lift } else { 0.0 },
            });
        }

        // static entities: group parts by (clustered) map position so all
        // parts of one building stand from the same floor line
        let rects = &normals;
        let mut reps: Vec<(i32, i32)> = Vec::new();
        let mut rep_cache: HashMap<(i32, i32), (i32, i32)> = HashMap::with_capacity(rects.len());
        // parts of one entity pass positions up to ~0.5 tile apart, so keys
        // within 0.75 tiles (192/256) join the same cluster
        let mut key_of = |r: &BbRect| -> (i32, i32) {
            if r.grp != 0 {
                return (r.grp as i32, i32::MIN);
            }
            let k = (r.kx, r.ky);
            if let Some(rep) = rep_cache.get(&k) {
                return *rep;
            }
            let rep = reps
                .iter()
                .copied()
                .find(|p| (p.0 - k.0).abs() <= 192 && (p.1 - k.1).abs() <= 192)
                .unwrap_or_else(|| {
                    reps.push(k);
                    k
                });
            rep_cache.insert(k, rep);
            rep
        };
        let mut feet: HashMap<(i32, i32), FeetGroup> = HashMap::with_capacity(rects.len());
        for r in rects {
            let vb = (r.cy + r.hh) / fh;
            let uc = r.cx / fw;
            feet.entry(key_of(r))
                .and_modify(|m| {
                    if vb > m.vb {
                        m.vb = vb;
                        m.uc = uc;
                    }
                    m.kx0 = m.kx0.min(r.kx);
                    m.kx1 = m.kx1.max(r.kx);
                    m.ky0 = m.ky0.min(r.ky);
                    m.ky1 = m.ky1.max(r.ky);
                })
                .or_insert(FeetGroup { vb, uc, kx0: r.kx, kx1: r.kx, ky0: r.ky, ky1: r.ky });
        }
        for r in rects {
            let u0 = ((r.cx - r.hw) / fw) * au + bu;
            let u1 = ((r.cx + r.hw) / fw) * au + bu;
            let v_top = ((r.cy - r.hh) / fh) * av + bv;
            let v_base = ((r.cy + r.hh) / fh) * av + bv;
            if u1 <= 0.0 || u0 >= 1.0 || v_base <= 0.02 || v_top >= 1.0 {
                continue;
            }
            // anchor at the entity's grouped feet — except elongated entities
            // (>3 tiles of part spread, e.g. diagonal trains), whose far end
            // would float; those ground each part at its own bottom
            let own_vb = (r.cy + r.hh) / fh;
            let (grp_vb, grp_uc) = match feet.get(&key_of(r)) {
                Some(g) => {
                    let spread = (g.kx1 - g.kx0).max(g.ky1 - g.ky0);
                    if spread > 768 { (own_vb, r.cx / fw) } else { (g.vb, g.uc) }
                }
                None => (own_vb, r.cx / fw),
            };
            let v_foot = (grp_vb * av + bv).clamp(0.0, 1.0);
            // a quad anchored at the plane's far edge stands into the sky
            if v_foot <= 0.02 {
                continue;
            }
            // skip floating banner parts (station name labels): much wider
            // than tall AND hovering well above the entity's feet
            if r.hw > 2.5 * r.hh && (v_foot - v_base) > 0.04 {
                continue;
            }
            // one hi tile per quad, chosen from the entity's anchor point so
            // every part of an entity samples the same tile/frame. moving
            // sprites use the live tile (handled above); static ones pick
            // their grid cell
            let sel = if !hi_ok {
                -1.0
            } else if r.special {
                let cov = tile_cover[LIVE_TILE];
                let ug = grp_uc * au + bu;
                if cov > 0.01 && (ug - 0.5).abs() < 0.5 * cov && (v_foot - 0.5).abs() < 0.5 * cov {
                    LIVE_TILE as f32
                } else {
                    -1.0
                }
            } else {
                let g = hi_grid as f32;
                let ug = grp_uc * au + bu;
                let tx = (ug * g).floor().clamp(0.0, g - 1.0);
                let ty = (v_foot * g).floor().clamp(0.0, g - 1.0);
                let ti = (ty as u32 * hi_grid + tx as u32) as usize;
                if tile_cover[ti] > 0.01 { ti as f32 } else { -1.0 }
            };
            billboards.push(BillboardUv {
                u0,
                u1,
                v_top,
                v_base,
                v_foot,
                sel,
                pu: 0.5 * (u0 + u1),
                flat: r.flat,
                fly_lift: if r.elevated_flat { platform_lift } else { 0.0 },
            });
        }
    }

    // sort far-to-near along the camera's forward direction (sorting by
    // v_foot alone is only correct at yaw 0). stable sort keeps same-entity
    // parts in record order so overlays still paint over their base
    let az = yaw.to_radians();
    let (sa, ca) = az.sin_cos();
    let aspect = fw / fh.max(1.0);
    let depth = |b: &BillboardUv| {
        let x = (b.pu - 0.5) * aspect;
        let z = 0.5 - b.v_foot;
        -sa * x + ca * z
    };
    billboards.sort_by(|a, b| depth(b).partial_cmp(&depth(a)).unwrap_or(std::cmp::Ordering::Equal));

    // over the cap: drop from the FRONT of the list so the nearest quads
    // survive (plain truncate dropped the near field, player included)
    if billboards.len() > settings::MAX_BILLBOARDS {
        let excess = billboards.len() - settings::MAX_BILLBOARDS;
        billboards.drain(0..excess);
    }
    billboards
}

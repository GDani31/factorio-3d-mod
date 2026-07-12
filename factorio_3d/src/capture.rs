// gpu layer capture.
//
// while a hooked drawEntities call runs, the world framebuffer is swapped
// for a transparent offscreen target, so the game rasterizes that layer
// range (objects, belts, elevated rails, wires) separately. the warp pass
// later composites the layers back in 3d.
//
// the game's state manager re-binds its own framebuffer mid-call, so the
// d3d11 context's OMSetRenderTargets is detoured: while a capture window is
// open, any bind of the world framebuffer is redirected to the capture
// target instead.

use retour::static_detour;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::Interface;

// renderer's verdict: will the warp run this frame? layers are only
// diverted when it does (the warp is what composites them back)
static CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);
// true while a diverted window is open (arms the bind redirect)
static DIVERT_ACTIVE: AtomicBool = AtomicBool::new(false);
// true only while the main GameRenderer::render pass runs
static WORLD_RENDER_ACTIVE: AtomicBool = AtomicBool::new(false);

// raw com pointers, valid only while a window is open (owners live in Capture)
static WORLD_RTV_RAW: AtomicUsize = AtomicUsize::new(0);
static WORLD_RES_RAW: AtomicUsize = AtomicUsize::new(0);
static TARGET_RTV_RAW: AtomicUsize = AtomicUsize::new(0);

pub fn set_capture_enabled(on: bool) {
    CAPTURE_ENABLED.store(on, Ordering::Relaxed);
}

pub fn capture_enabled() -> bool {
    CAPTURE_ENABLED.load(Ordering::Relaxed)
}

pub fn set_world_render_active(on: bool) {
    WORLD_RENDER_ACTIVE.store(on, Ordering::Relaxed);
}

pub fn world_render_active() -> bool {
    WORLD_RENDER_ACTIVE.load(Ordering::Relaxed)
}

// which layer group a diverted call belongs to
#[derive(Clone, Copy, PartialEq)]
pub enum CaptureKind {
    // buildings/characters/vehicles (layers 120..133) -> standing billboards
    Object,
    // retagged belt/rail sprites -> lifted flat plane
    Belt,
    // elevated rails + trains on them (133..153) -> high floating plane
    Elevated,
    // wires (layer 131) -> plane at pole height (they span between poles,
    // so per-entity billboards can't work)
    Wire,
}

// which hi-res replay target a window feeds
#[derive(Clone, Copy, PartialEq)]
pub enum HiKind {
    Object,
    Ground,
    Belt,
    Elevated,
}

// hi-res targets are this many times the fbo size per axis. pinned at 2.0 —
// empirically the only size the game's pipeline places correctly; coverage
// comes from tiling instead
pub const HI_SUPER: f32 = 2.0;
// upper bound on the hi tiling grid per axis
pub const MAX_HI_GRID: u32 = 2;
pub const MAX_HI_TILES: usize = (MAX_HI_GRID * MAX_HI_GRID) as usize;
// extra slice re-rendered EVERY frame, centered — moving sprites sample it
// (the rotated grid tiles are up to grid^2-1 frames stale)
pub const LIVE_TILE: usize = MAX_HI_TILES;
pub const MAX_HI_SLICES: usize = MAX_HI_TILES + 1;

// tiles per axis so the grid of HI_SUPER/boost windows covers the view
pub fn hi_grid_for(boost: f32) -> u32 {
    ((boost / HI_SUPER).ceil() as u32).clamp(1, MAX_HI_GRID)
}

// one hi tile's stamp: the view rect (tiles) + per-axis coverage it was
// rendered under. cover == 0 means never rendered / invalidated
#[derive(Clone, Copy, Default)]
pub struct HiTileMeta {
    pub stamp: (f32, f32, f32, f32),
    pub cover: f32,
}

// frame counter for the tile rotation (one tile is re-rendered per frame)
static HI_ROT_FRAME: AtomicU64 = AtomicU64::new(0);
static HI_LAST_GRID: AtomicUsize = AtomicUsize::new(0);
static HI_TILE_META: Mutex<[HiTileMeta; MAX_HI_SLICES]> =
    Mutex::new([HiTileMeta { stamp: (0.0, 0.0, 0.0, 0.0), cover: 0.0 }; MAX_HI_SLICES]);

// advance the tile rotation — called once per frame
pub fn advance_hi_rotation() {
    HI_ROT_FRAME.fetch_add(1, Ordering::Relaxed);
}

// a basic capture target (one texture, fbo-sized)
struct Target {
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    cleared: bool,
    captured: bool,
}

// a tiled hi-res target: a Texture2DArray with one HI_SUPER x fbo slice per
// tile; each holds one window of the view rasterized at full sharpness
struct HiTarget {
    rtvs: Vec<ID3D11RenderTargetView>,
    srv: ID3D11ShaderResourceView,
    w: u32,
    h: u32,
    slices: u32,
    format: DXGI_FORMAT,
    cleared: bool,
    live_cleared: bool,
    captured: bool,
}

struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    object: Option<Target>,
    belt: Option<Target>,
    elevated: Option<Target>,
    wire: Option<Target>,
    object_hi: Option<HiTarget>,
    ground_hi: Option<HiTarget>,
    belt_hi: Option<HiTarget>,
    elevated_hi: Option<HiTarget>,
    hi_grid: u32,
    // world fbo bindings saved across one diverted call
    saved_rtv: Option<ID3D11RenderTargetView>,
    saved_dsv: Option<ID3D11DepthStencilView>,
    saved_res: Option<ID3D11Resource>,
    saved_vp: Option<D3D11_VIEWPORT>,
    active_kind: Option<CaptureKind>,
    hi_window: Option<HiKind>,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);

type FnOmSetRenderTargets = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    u32,
    *const *mut core::ffi::c_void,
    *mut core::ffi::c_void,
);
type FnRsSetViewports =
    unsafe extern "system" fn(*mut core::ffi::c_void, u32, *const D3D11_VIEWPORT);
type FnRsSetScissorRects = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    u32,
    *const windows::Win32::Foundation::RECT,
);

static_detour! {
    static OmSetRenderTargetsHook: unsafe extern "system" fn(*mut core::ffi::c_void, u32, *const *mut core::ffi::c_void, *mut core::ffi::c_void);
    static RsSetViewportsHook: unsafe extern "system" fn(*mut core::ffi::c_void, u32, *const D3D11_VIEWPORT);
    static RsSetScissorRectsHook: unsafe extern "system" fn(*mut core::ffi::c_void, u32, *const windows::Win32::Foundation::RECT);
}

// while a hi window is open: (base full-viewport width, magnified viewport).
// the game re-sets a full-size viewport mid-call; this keeps the
// magnification alive
static HI_VP_SUB: Mutex<Option<(f32, D3D11_VIEWPORT)>> = Mutex::new(None);
// same for the scissor rect (an fbo-sized scissor would clip the tile)
static HI_SCISSOR_SUB: Mutex<Option<(i32, windows::Win32::Foundation::RECT)>> = Mutex::new(None);

fn hooked_om_set_render_targets(
    this: *mut core::ffi::c_void,
    num_views: u32,
    rtvs: *const *mut core::ffi::c_void,
    dsv: *mut core::ffi::c_void,
) {
    if DIVERT_ACTIVE.load(Ordering::Relaxed) && num_views >= 1 && !rtvs.is_null() {
        let first = unsafe { *rtvs } as usize;
        let world_rtv = WORLD_RTV_RAW.load(Ordering::Relaxed);
        // fast path: same rtv. slow path: a different rtv over the same texture
        let mut is_world = world_rtv != 0 && first == world_rtv;
        if !is_world && first != 0 {
            let world_res = WORLD_RES_RAW.load(Ordering::Relaxed);
            if world_res != 0 {
                unsafe {
                    let ptr = first as *mut core::ffi::c_void;
                    if let Some(rtv) = ID3D11RenderTargetView::from_raw_borrowed(&ptr) {
                        if let Ok(res) = rtv.GetResource() {
                            is_world = res.as_raw() as usize == world_res;
                        }
                    }
                }
            }
        }
        if is_world {
            let target = TARGET_RTV_RAW.load(Ordering::Relaxed);
            if target != 0 {
                let n = (num_views as usize).min(8);
                let mut arr: [*mut core::ffi::c_void; 8] = [std::ptr::null_mut(); 8];
                for (i, slot) in arr.iter_mut().enumerate().take(n) {
                    *slot = unsafe { *rtvs.add(i) };
                }
                arr[0] = target as *mut core::ffi::c_void;
                unsafe { OmSetRenderTargetsHook.call(this, num_views, arr.as_ptr(), dsv) };
                return;
            }
        }
    }
    unsafe { OmSetRenderTargetsHook.call(this, num_views, rtvs, dsv) }
}

fn hooked_rs_set_viewports(this: *mut core::ffi::c_void, num: u32, vps: *const D3D11_VIEWPORT) {
    if num >= 1 && !vps.is_null() {
        if let Ok(guard) = HI_VP_SUB.lock() {
            if let Some((base_w, scaled)) = *guard {
                let v = unsafe { *vps };
                if (v.Width - base_w).abs() < 1.5 && v.TopLeftX.abs() < 1.5 {
                    let mut arr = [D3D11_VIEWPORT::default(); 8];
                    let n = (num as usize).min(8);
                    for (i, slot) in arr.iter_mut().enumerate().take(n) {
                        *slot = unsafe { *vps.add(i) };
                    }
                    arr[0] = scaled;
                    unsafe { RsSetViewportsHook.call(this, num, arr.as_ptr()) };
                    return;
                }
            }
        }
    }
    unsafe { RsSetViewportsHook.call(this, num, vps) }
}

fn hooked_rs_set_scissor_rects(
    this: *mut core::ffi::c_void,
    num: u32,
    rects: *const windows::Win32::Foundation::RECT,
) {
    if num >= 1 && !rects.is_null() {
        if let Ok(guard) = HI_SCISSOR_SUB.lock() {
            if let Some((base_w, full)) = *guard {
                let r = unsafe { *rects };
                if (r.right - r.left - base_w).abs() <= 2 && r.left.abs() <= 2 {
                    let mut arr = [windows::Win32::Foundation::RECT::default(); 8];
                    let n = (num as usize).min(8);
                    for (i, slot) in arr.iter_mut().enumerate().take(n) {
                        *slot = unsafe { *rects.add(i) };
                    }
                    arr[0] = full;
                    unsafe { RsSetScissorRectsHook.call(this, num, arr.as_ptr()) };
                    return;
                }
            }
        }
    }
    unsafe { RsSetScissorRectsHook.call(this, num, rects) }
}

// install the context-vtable detours and set up the capture state.
// called once by the renderer when d3d11 comes up
pub fn init(device: ID3D11Device, context: ID3D11DeviceContext) {
    unsafe {
        let raw = context.as_raw();
        let vtbl = *(raw as *const *const usize);
        // ID3D11DeviceContext vtable slots: OMSetRenderTargets=33,
        // RSSetViewports=44, RSSetScissorRects=45
        let om: FnOmSetRenderTargets = std::mem::transmute(*vtbl.add(33));
        match OmSetRenderTargetsHook.initialize(om, hooked_om_set_render_targets) {
            Ok(h) => {
                if let Err(e) = h.enable() {
                    log::warn!("[capture] OMSetRenderTargets enable failed: {e}");
                }
            }
            Err(e) => log::warn!("[capture] OMSetRenderTargets detour failed: {e}"),
        }
        let vp: FnRsSetViewports = std::mem::transmute(*vtbl.add(44));
        match RsSetViewportsHook.initialize(vp, hooked_rs_set_viewports) {
            Ok(h) => {
                if let Err(e) = h.enable() {
                    log::warn!("[capture] RSSetViewports enable failed: {e}");
                }
            }
            Err(e) => log::warn!("[capture] RSSetViewports detour failed: {e}"),
        }
        let sc: FnRsSetScissorRects = std::mem::transmute(*vtbl.add(45));
        match RsSetScissorRectsHook.initialize(sc, hooked_rs_set_scissor_rects) {
            Ok(h) => {
                if let Err(e) = h.enable() {
                    log::warn!("[capture] RSSetScissorRects enable failed: {e}");
                }
            }
            Err(e) => log::warn!("[capture] RSSetScissorRects detour failed: {e}"),
        }
    }

    *CAPTURE.lock().unwrap() = Some(Capture {
        device,
        context,
        object: None,
        belt: None,
        elevated: None,
        wire: None,
        object_hi: None,
        ground_hi: None,
        belt_hi: None,
        elevated_hi: None,
        hi_grid: 1,
        saved_rtv: None,
        saved_dsv: None,
        saved_res: None,
        saved_vp: None,
        active_kind: None,
        hi_window: None,
    });
    log::info!("[capture] initialized");
}

// swap the capture target in for the currently bound world fbo. returns
// false (state untouched) if anything is off — the call then runs vanilla
pub fn begin_capture(kind: CaptureKind) -> bool {
    let mut guard = match CAPTURE.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let cap = match guard.as_mut() {
        Some(c) => c,
        None => return false,
    };
    if cap.saved_rtv.is_some() {
        return false; // begin without end — refuse
    }
    unsafe {
        let mut rtvs = [None; 1];
        let mut dsv = None;
        cap.context.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        let rtv = match rtvs[0].take() {
            Some(r) => r,
            None => return false,
        };
        let resource = match rtv.GetResource() {
            Ok(r) => r,
            Err(_) => return false,
        };
        let tex: ID3D11Texture2D = match resource.cast() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);

        // only divert draws into the world fbo (it's at least window-sized;
        // entity previews/minimaps use small targets)
        let base_w = crate::hooks::frame::base_size().0;
        if base_w == 0 || desc.Width < base_w {
            return false;
        }

        if !ensure_target(cap, kind, desc.Width, desc.Height, desc.Format) {
            return false;
        }
        let context = cap.context.clone();
        let target = match kind {
            CaptureKind::Object => cap.object.as_mut().unwrap(),
            CaptureKind::Belt => cap.belt.as_mut().unwrap(),
            CaptureKind::Elevated => cap.elevated.as_mut().unwrap(),
            CaptureKind::Wire => cap.wire.as_mut().unwrap(),
        };
        if !target.cleared {
            context.ClearRenderTargetView(&target.rtv, &[0.0f32, 0.0, 0.0, 0.0]);
            target.cleared = true;
        }
        // same dsv stays bound (same size, so the pairing is legal)
        context.OMSetRenderTargets(Some(&[Some(target.rtv.clone())]), dsv.as_ref());

        WORLD_RTV_RAW.store(rtv.as_raw() as usize, Ordering::Relaxed);
        WORLD_RES_RAW.store(resource.as_raw() as usize, Ordering::Relaxed);
        TARGET_RTV_RAW.store(target.rtv.as_raw() as usize, Ordering::Relaxed);
        cap.saved_rtv = Some(rtv);
        cap.saved_dsv = dsv;
        cap.saved_res = Some(resource);
        cap.active_kind = Some(kind);
        DIVERT_ACTIVE.store(true, Ordering::Release);
    }
    true
}

// restore the world fbo after a diverted call
pub fn end_capture() {
    let mut guard = match CAPTURE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let cap = match guard.as_mut() {
        Some(c) => c,
        None => return,
    };
    // disarm the redirect first, so our own restore bind isn't redirected
    DIVERT_ACTIVE.store(false, Ordering::Release);
    WORLD_RTV_RAW.store(0, Ordering::Relaxed);
    WORLD_RES_RAW.store(0, Ordering::Relaxed);
    TARGET_RTV_RAW.store(0, Ordering::Relaxed);
    if let Some(rtv) = cap.saved_rtv.take() {
        let dsv = cap.saved_dsv.take();
        unsafe {
            cap.context.OMSetRenderTargets(Some(&[Some(rtv)]), dsv.as_ref());
        }
        cap.saved_res = None;
        let target = match cap.active_kind.take() {
            Some(CaptureKind::Object) => cap.object.as_mut(),
            Some(CaptureKind::Belt) => cap.belt.as_mut(),
            Some(CaptureKind::Elevated) => cap.elevated.as_mut(),
            Some(CaptureKind::Wire) => cap.wire.as_mut(),
            None => None,
        };
        if let Some(t) = target {
            t.captured = true;
        }
    }
}

// open a second window for the same draw calls, into this frame's rotation
// tile of the hi-res array, with a boost-scaled viewport: the rasterizer
// magnifies everything by `boost`, so the sampler reads sharp mip levels —
// vanilla-resolution pixels for that tile's window of the boosted world.
// `live` renders the every-frame centered tile instead (object target only)
pub fn begin_hi_capture(kind: HiKind, boost: f32, grid: u32, live: bool) -> bool {
    if !(1.01..=16.0).contains(&boost) || grid == 0 || grid > MAX_HI_GRID {
        return false;
    }
    let n = (HI_ROT_FRAME.load(Ordering::Relaxed) % (grid as u64 * grid as u64)) as u32;
    let (tx, ty) = (n % grid, n / grid);
    // grid changed (zoom animation): tile indices mean something new —
    // invalidate all stamps so stale tiles fall back to low-res
    if HI_LAST_GRID.swap(grid as usize, Ordering::Relaxed) != grid as usize {
        if let Ok(mut m) = HI_TILE_META.lock() {
            *m = [HiTileMeta::default(); MAX_HI_SLICES];
        }
    }
    let mut guard = match CAPTURE.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let cap = match guard.as_mut() {
        Some(c) => c,
        None => return false,
    };
    if cap.saved_rtv.is_some() {
        return false;
    }
    unsafe {
        let mut rtvs = [None; 1];
        let mut dsv = None;
        cap.context.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        let rtv = match rtvs[0].take() {
            Some(r) => r,
            None => return false,
        };
        let resource = match rtv.GetResource() {
            Ok(r) => r,
            Err(_) => return false,
        };
        let tex: ID3D11Texture2D = match resource.cast() {
            Ok(t) => t,
            Err(_) => return false,
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        let base_w = crate::hooks::frame::base_size().0;
        if base_w == 0 || desc.Width < base_w {
            return false;
        }
        let hw = desc.Width * HI_SUPER as u32;
        let hh = desc.Height * HI_SUPER as u32;
        // full slice set always allocated, so grid flips never reallocate.
        // only the object target carries the extra live slice (moving sprites)
        let slices = match kind {
            HiKind::Object => MAX_HI_SLICES as u32,
            _ => MAX_HI_TILES as u32,
        };
        {
            let Capture { device, object_hi, ground_hi, belt_hi, elevated_hi, .. } = cap;
            let (slot, label) = match kind {
                HiKind::Object => (object_hi, "object-hi"),
                HiKind::Ground => (ground_hi, "ground-hi"),
                HiKind::Belt => (belt_hi, "belt-hi"),
                HiKind::Elevated => (elevated_hi, "elevated-hi"),
            };
            if !ensure_hi_slot(device, slot, label, hw, hh, slices, desc.Format) {
                return false;
            }
        }
        let context = cap.context.clone();
        let target = match kind {
            HiKind::Object => cap.object_hi.as_mut().unwrap(),
            HiKind::Ground => cap.ground_hi.as_mut().unwrap(),
            HiKind::Belt => cap.belt_hi.as_mut().unwrap(),
            HiKind::Elevated => cap.elevated_hi.as_mut().unwrap(),
        };
        if live && kind != HiKind::Object {
            return false;
        }
        let slice = if live { LIVE_TILE } else { (ty * grid + tx) as usize };
        if slice >= target.rtvs.len() {
            return false;
        }
        let clear_flag = if live { &mut target.live_cleared } else { &mut target.cleared };
        if !*clear_flag {
            // clear only this window's tile; others keep their stamped content
            context.ClearRenderTargetView(&target.rtvs[slice], &[0.0f32, 0.0, 0.0, 0.0]);
            *clear_flag = true;
        }
        let tile_rtv = target.rtvs[slice].clone();
        if let Ok(mut m) = HI_TILE_META.lock() {
            m[slice] = HiTileMeta {
                stamp: crate::hooks::frame::view_rect_tiles(),
                cover: HI_SUPER / boost,
            };
        }
        // no dsv: the hi target's size differs from the world depth buffer
        context.OMSetRenderTargets(Some(&[Some(tile_rtv.clone())]), None);

        // magnified viewport, placed so this tile's view-uv center lands at
        // the tile center. do NOT anchor on player position or yaw — both
        // were tried and regressed in-game
        let mut num = 1u32;
        let mut vps = [D3D11_VIEWPORT::default(); 1];
        cap.context.RSGetViewports(&mut num, Some(vps.as_mut_ptr()));
        let base_vp = if num >= 1 {
            vps[0]
        } else {
            D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: desc.Width as f32,
                Height: desc.Height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }
        };
        let w = desc.Width as f32;
        let h = desc.Height as f32;
        let cu = if live { 0.5 } else { (tx as f32 + 0.5) / grid as f32 };
        let cv = if live { 0.5 } else { (ty as f32 + 0.5) / grid as f32 };
        let scaled = D3D11_VIEWPORT {
            TopLeftX: HI_SUPER * w * 0.5 - cu * boost * w,
            TopLeftY: HI_SUPER * h * 0.5 - cv * boost * h,
            Width: w * boost,
            Height: h * boost,
            MinDepth: base_vp.MinDepth,
            MaxDepth: base_vp.MaxDepth,
        };
        cap.context.RSSetViewports(Some(&[scaled]));
        if let Ok(mut sub) = HI_VP_SUB.lock() {
            *sub = Some((w, scaled));
        }
        // widen the scissor to the whole tile (and keep it widened)
        let full = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: hw as i32,
            bottom: hh as i32,
        };
        cap.context.RSSetScissorRects(Some(&[full]));
        if let Ok(mut sub) = HI_SCISSOR_SUB.lock() {
            *sub = Some((w as i32, full));
        }

        WORLD_RTV_RAW.store(rtv.as_raw() as usize, Ordering::Relaxed);
        WORLD_RES_RAW.store(resource.as_raw() as usize, Ordering::Relaxed);
        TARGET_RTV_RAW.store(tile_rtv.as_raw() as usize, Ordering::Relaxed);
        cap.saved_rtv = Some(rtv);
        cap.saved_dsv = dsv;
        cap.saved_res = Some(resource);
        cap.saved_vp = Some(base_vp);
        cap.active_kind = None;
        cap.hi_window = Some(kind);
        cap.hi_grid = grid;
        DIVERT_ACTIVE.store(true, Ordering::Release);
    }
    true
}

// close the hi window: restore the world fbo, viewport and scissor
pub fn end_hi_capture() {
    let mut guard = match CAPTURE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let cap = match guard.as_mut() {
        Some(c) => c,
        None => return,
    };
    DIVERT_ACTIVE.store(false, Ordering::Release);
    if let Ok(mut sub) = HI_VP_SUB.lock() {
        *sub = None;
    }
    if let Ok(mut sub) = HI_SCISSOR_SUB.lock() {
        *sub = None;
    }
    // restore an fbo-sized scissor for the world render
    let (base_w, base_h) = crate::hooks::frame::base_size();
    if base_w > 0 && base_h > 0 {
        unsafe {
            cap.context.RSSetScissorRects(Some(&[windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: base_w as i32,
                bottom: base_h as i32,
            }]));
        }
    }
    WORLD_RTV_RAW.store(0, Ordering::Relaxed);
    WORLD_RES_RAW.store(0, Ordering::Relaxed);
    TARGET_RTV_RAW.store(0, Ordering::Relaxed);
    if let Some(rtv) = cap.saved_rtv.take() {
        let dsv = cap.saved_dsv.take();
        unsafe {
            cap.context.OMSetRenderTargets(Some(&[Some(rtv)]), dsv.as_ref());
            if let Some(vp) = cap.saved_vp.take() {
                cap.context.RSSetViewports(Some(&[vp]));
            }
        }
        cap.saved_res = None;
        cap.active_kind = None;
        let target = match cap.hi_window.take() {
            Some(HiKind::Object) => cap.object_hi.as_mut(),
            Some(HiKind::Ground) => cap.ground_hi.as_mut(),
            Some(HiKind::Belt) => cap.belt_hi.as_mut(),
            Some(HiKind::Elevated) => cap.elevated_hi.as_mut(),
            None => None,
        };
        if let Some(t) = target {
            t.captured = true;
        }
    }
}

// what one frame's diverted layers produced
pub struct FrameCapture {
    pub object: Option<ID3D11ShaderResourceView>,
    // Texture2DArray srvs, one slice per hi tile (validity lives in hi_meta)
    pub object_hi: Option<ID3D11ShaderResourceView>,
    pub ground_hi: Option<ID3D11ShaderResourceView>,
    pub belt_hi: Option<ID3D11ShaderResourceView>,
    pub elevated_hi: Option<ID3D11ShaderResourceView>,
    pub hi_grid: u32,
    pub hi_meta: [HiTileMeta; MAX_HI_SLICES],
    pub belt: Option<ID3D11ShaderResourceView>,
    pub elevated: Option<ID3D11ShaderResourceView>,
    pub wire: Option<ID3D11ShaderResourceView>,
    pub w: u32,
    pub h: u32,
}

// end-of-frame harvest; resets the per-frame flags
pub fn take_frame_capture() -> Option<FrameCapture> {
    let mut guard = CAPTURE.lock().ok()?;
    let cap = guard.as_mut()?;
    // safety net: a begin without end must not leak into the next frame
    DIVERT_ACTIVE.store(false, Ordering::Release);
    cap.active_kind = None;
    if let Some(rtv) = cap.saved_rtv.take() {
        let dsv = cap.saved_dsv.take();
        unsafe {
            cap.context.OMSetRenderTargets(Some(&[Some(rtv)]), dsv.as_ref());
        }
        cap.saved_res = None;
        WORLD_RTV_RAW.store(0, Ordering::Relaxed);
        WORLD_RES_RAW.store(0, Ordering::Relaxed);
        TARGET_RTV_RAW.store(0, Ordering::Relaxed);
    }
    let mut result = FrameCapture {
        object: None,
        object_hi: None,
        ground_hi: None,
        belt_hi: None,
        elevated_hi: None,
        hi_grid: cap.hi_grid.max(1),
        hi_meta: HI_TILE_META
            .lock()
            .map(|m| *m)
            .unwrap_or([HiTileMeta::default(); MAX_HI_SLICES]),
        belt: None,
        elevated: None,
        wire: None,
        w: 0,
        h: 0,
    };
    for (t, slot) in [
        (cap.object.as_mut(), 0usize),
        (cap.belt.as_mut(), 1),
        (cap.elevated.as_mut(), 2),
        (cap.wire.as_mut(), 3),
    ] {
        if let Some(t) = t {
            t.cleared = false;
            if std::mem::take(&mut t.captured) {
                if result.w == 0 {
                    result.w = t.w;
                    result.h = t.h;
                }
                match slot {
                    0 => result.object = Some(t.srv.clone()),
                    1 => result.belt = Some(t.srv.clone()),
                    2 => result.elevated = Some(t.srv.clone()),
                    _ => result.wire = Some(t.srv.clone()),
                }
            }
        }
    }
    // hi tiles are temporal: srvs are useful whenever the targets exist
    for (t, slot) in [
        (cap.object_hi.as_mut(), 0usize),
        (cap.ground_hi.as_mut(), 1),
        (cap.belt_hi.as_mut(), 2),
        (cap.elevated_hi.as_mut(), 3),
    ] {
        if let Some(t) = t {
            t.cleared = false;
            t.live_cleared = false;
            t.captured = false;
            match slot {
                0 => result.object_hi = Some(t.srv.clone()),
                1 => result.ground_hi = Some(t.srv.clone()),
                2 => result.belt_hi = Some(t.srv.clone()),
                _ => result.elevated_hi = Some(t.srv.clone()),
            }
        }
    }
    if result.object.is_none()
        && result.belt.is_none()
        && result.elevated.is_none()
        && result.wire.is_none()
    {
        return None;
    }
    Some(result)
}

fn ensure_target(cap: &mut Capture, kind: CaptureKind, w: u32, h: u32, format: DXGI_FORMAT) -> bool {
    let Capture { device, object, belt, elevated, wire, .. } = cap;
    let (slot, label) = match kind {
        CaptureKind::Object => (object, "object"),
        CaptureKind::Belt => (belt, "belt"),
        CaptureKind::Elevated => (elevated, "elevated"),
        CaptureKind::Wire => (wire, "wire"),
    };
    ensure_slot(device, slot, label, w, h, format)
}

// (re)create one capture target to match the given size/format
fn ensure_slot(
    device: &ID3D11Device,
    slot: &mut Option<Target>,
    label: &str,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> bool {
    if let Some(t) = slot.as_ref() {
        if t.w == w && t.h == h && t.format == format {
            return true;
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w.max(1),
        Height: h.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    unsafe {
        let mut tex = None;
        if device.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
            return false;
        }
        let tex = match tex {
            Some(t) => t,
            None => return false,
        };
        let mut rtv = None;
        if device.CreateRenderTargetView(&tex, None, Some(&mut rtv)).is_err() {
            return false;
        }
        let mut srv = None;
        if device.CreateShaderResourceView(&tex, None, Some(&mut srv)).is_err() {
            return false;
        }
        match (rtv, srv) {
            (Some(rtv), Some(srv)) => {
                log::info!("[capture] {label} target {w}x{h} created");
                *slot = Some(Target { rtv, srv, w, h, format, cleared: false, captured: false });
                true
            }
            _ => false,
        }
    }
}

// (re)create a tiled hi-res slot: a Texture2DArray, one rtv per slice,
// one array srv over all (always an array so the hlsl declaration matches)
fn ensure_hi_slot(
    device: &ID3D11Device,
    slot: &mut Option<HiTarget>,
    label: &str,
    w: u32,
    h: u32,
    slices: u32,
    format: DXGI_FORMAT,
) -> bool {
    if let Some(t) = slot.as_ref() {
        if t.w == w && t.h == h && t.slices == slices && t.format == format {
            return true;
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w.max(1),
        Height: h.max(1),
        MipLevels: 1,
        ArraySize: slices.max(1),
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    unsafe {
        let mut tex = None;
        if device.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
            return false;
        }
        let tex = match tex {
            Some(t) => t,
            None => return false,
        };
        let mut rtvs = Vec::with_capacity(slices as usize);
        for i in 0..slices.max(1) {
            let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                        MipSlice: 0,
                        FirstArraySlice: i,
                        ArraySize: 1,
                    },
                },
            };
            let mut rtv = None;
            if device.CreateRenderTargetView(&tex, Some(&rtv_desc), Some(&mut rtv)).is_err() {
                return false;
            }
            match rtv {
                Some(r) => rtvs.push(r),
                None => return false,
            }
        }
        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: 0,
                    ArraySize: slices.max(1),
                },
            },
        };
        let mut srv = None;
        if device.CreateShaderResourceView(&tex, Some(&srv_desc), Some(&mut srv)).is_err() {
            return false;
        }
        match srv {
            Some(srv) => {
                log::info!("[capture] {label} target {w}x{h} x{slices} tiles created");
                *slot = Some(HiTarget {
                    rtvs,
                    srv,
                    w,
                    h,
                    slices,
                    format,
                    cleared: false,
                    live_cleared: false,
                    captured: false,
                });
                true
            }
            None => false,
        }
    }
}

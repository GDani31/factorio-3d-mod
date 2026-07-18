// per-frame hooks: dxgi Present, the world render pass, the render params
// that decide the visible world rect, and the horizon zoom boost.

use crate::offsets;
use crate::renderer::Renderer3D;
use crate::settings;
use crate::symbols::SymbolMap;
use crate::util::AtomicF32;
use anyhow::{Context, Result};
use retour::static_detour;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::D3D11CreateDeviceAndSwapChain;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGISwapChain,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExA, DefWindowProcA, DestroyWindow, RegisterClassExA,
    WINDOW_EX_STYLE, WNDCLASSEXA, WS_OVERLAPPEDWINDOW,
};
use windows::core::Interface;

static RENDERER: OnceLock<Renderer3D> = OnceLock::new();

// vanilla window/back-buffer render size
static BASE_W: AtomicU32 = AtomicU32::new(0);
static BASE_H: AtomicU32 = AtomicU32::new(0);

// frame counter (bumped once per world render)
static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

// zoom boost actually applied to this frame's render
static ZOOM_BOOST: AtomicF32 = AtomicF32::new(1.0);

// view rect of the current main-view render, in tiles
static RECT_L: AtomicF32 = AtomicF32::new(0.0);
static RECT_T: AtomicF32 = AtomicF32::new(0.0);
static SPAN_X: AtomicF32 = AtomicF32::new(0.0);
static SPAN_Y: AtomicF32 = AtomicF32::new(0.0);

thread_local! {
    // true while GameView::createRenderParameters runs (gates the boost)
    static MAIN_RP_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // boost the centerOn hook applied inside the current createRenderParameters
    static RP_BOOST: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
}

pub fn create_renderer() -> Result<()> {
    RENDERER.set(Renderer3D::new()?).ok();
    Ok(())
}

pub fn set_base_size(w: u32, h: u32) {
    BASE_W.store(w, Ordering::Relaxed);
    BASE_H.store(h, Ordering::Relaxed);
}

pub fn base_size() -> (u32, u32) {
    (BASE_W.load(Ordering::Relaxed), BASE_H.load(Ordering::Relaxed))
}

pub fn frame_count() -> u64 {
    FRAME_COUNT.load(Ordering::Relaxed)
}

pub fn zoom_boost_applied() -> f32 {
    ZOOM_BOOST.get()
}

// main view rect: (left, top, span_x, span_y) in tiles
pub fn view_rect_tiles() -> (f32, f32, f32, f32) {
    (RECT_L.get(), RECT_T.get(), SPAN_X.get(), SPAN_Y.get())
}

type FnPresent = unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32) -> i32;
type FnRender = unsafe extern "C" fn(*mut core::ffi::c_void);
// createRenderParameters — struct return via hidden pointer
type FnCreateRp = unsafe extern "C" fn(
    *mut core::ffi::c_void,
    *mut core::ffi::c_void,
) -> *mut core::ffi::c_void;
type FnAckResize = unsafe extern "C" fn(*mut core::ffi::c_void, u16, u16);
// centerOn(Map&, SurfaceView&); r9/rax forwarded defensively
type FnCenterOn = unsafe extern "C" fn(
    *mut core::ffi::c_void,
    *const core::ffi::c_void,
    *const core::ffi::c_void,
    usize,
) -> usize;

static_detour! {
    static PresentHook: unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32) -> i32;
    static RenderHook: unsafe extern "C" fn(*mut core::ffi::c_void);
    static CreateRpHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    static AckResizeHook: unsafe extern "C" fn(*mut core::ffi::c_void, u16, u16);
    static CenterOnHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const core::ffi::c_void, usize) -> usize;
}

// present + world render hooks
pub fn install_early(symbols: &SymbolMap, base: usize) -> Result<()> {
    install_present()?;

    let addr = super::resolve(symbols, base, &offsets::GAME_RENDERER_RENDER);
    unsafe {
        let target: FnRender = std::mem::transmute(addr);
        RenderHook.initialize(target, hooked_render)?.enable()?;
    }
    Ok(())
}

// render-parameter hooks (view rect, resize tracking, zoom boost)
pub fn install_params(symbols: &SymbolMap, base: usize) -> Result<()> {
    let addr = super::resolve(symbols, base, &offsets::CREATE_RENDER_PARAMS);
    unsafe {
        let target: FnCreateRp = std::mem::transmute(addr);
        CreateRpHook.initialize(target, hooked_create_rp)?.enable()?;
    }
    let addr = super::resolve(symbols, base, &offsets::GAME_RENDERER_ACK_RESIZE);
    unsafe {
        let target: FnAckResize = std::mem::transmute(addr);
        AckResizeHook.initialize(target, hooked_ack_resize)?.enable()?;
    }
    let addr = super::resolve(symbols, base, &offsets::RP_CENTER_ON);
    unsafe {
        let target: FnCenterOn = std::mem::transmute(addr);
        CenterOnHook.initialize(target, hooked_center_on)?.enable()?;
    }
    Ok(())
}

// --- dxgi Present ---------------------------------------------------------------

fn install_present() -> Result<()> {
    let addr = unsafe { find_present_address()? };
    if addr == 0 {
        anyhow::bail!("Present address is null");
    }
    unsafe {
        let target: FnPresent = std::mem::transmute(addr);
        PresentHook.initialize(target, hooked_present)?.enable()?;
    }
    log::info!("dxgi Present hook at 0x{addr:X}");
    Ok(())
}

fn hooked_present(this: *mut core::ffi::c_void, sync_interval: u32, flags: u32) -> i32 {
    if let Some(renderer) = RENDERER.get() {
        super::guard("present", (), || renderer.on_present(this));
    }
    unsafe { PresentHook.call(this, sync_interval, flags) }
}

unsafe extern "system" fn dummy_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { DefWindowProcA(hwnd, msg, wparam, lparam) }
}

// Present's address is read from a dummy swap chain's vtable (com layout:
// interface -> vtable -> function pointers; Present is index 8)
unsafe fn find_present_address() -> Result<usize> {
    unsafe {
        let class_name = windows::core::s!("f3dm_tmp");
        let wc = WNDCLASSEXA {
            cbSize: std::mem::size_of::<WNDCLASSEXA>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(dummy_wndproc),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExA(&wc);
        let hwnd: HWND = CreateWindowExA(
            WINDOW_EX_STYLE::default(),
            class_name,
            windows::core::s!(""),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            2,
            2,
            None,
            None,
            None,
            None,
        )?;

        let sd = DXGI_SWAP_CHAIN_DESC {
            BufferCount: 1,
            BufferDesc: DXGI_MODE_DESC {
                Width: 2,
                Height: 2,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            OutputWindow: hwnd,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
            ..Default::default()
        };
        let mut swapchain: Option<IDXGISwapChain> = None;
        let mut device = None;
        let mut context = None;
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            Default::default(),
            None,
            7, // D3D11_SDK_VERSION
            Some(&sd as *const _),
            Some(&mut swapchain),
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDeviceAndSwapChain failed")?;
        let swapchain = swapchain.context("no swap chain")?;

        let interface_ptr = swapchain.as_raw() as *const *const *const ();
        let vtable = *interface_ptr;
        let present = *vtable.add(8) as usize;

        drop(swapchain);
        drop(context);
        drop(device);
        let _ = DestroyWindow(hwnd);
        Ok(present)
    }
}

// --- world render bracketing ------------------------------------------------------

fn hooked_render(this: *mut core::ffi::c_void) {
    unsafe { RenderHook.call(this) }
    let frame = FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
    // chunks rendered before injection (or before the models loaded) sit in
    // the render cache and never call the entity draw hooks. nudge the
    // renderer with a same-size "resize" a few seconds in — that re-queues
    // every chunk, so belts/walls/pipes get recorded without a mouse hover
    const REQUEUE_NUDGE_FRAMES: [u64; 2] = [300, 1200];
    if REQUEUE_NUDGE_FRAMES.contains(&frame) {
        let (w, h) = base_size();
        if w > 0 && h > 0 {
            log::info!("[frame] re-queue nudge (frame {frame})");
            unsafe { AckResizeHook.call(this, w as u16, h as u16) }
        }
    }
    if let Some(renderer) = RENDERER.get() {
        super::guard("world_render", (), || renderer.on_after_world_render());
    }
}

// --- render parameters --------------------------------------------------------------

static BOOST_LOGGED: AtomicBool = AtomicBool::new(false);

// other GameViews (entity previews in guis) also build RenderParameters.
// the main view is told apart by pixel width: previews are far smaller than
// half the window (an unknown base size counts as main)
fn rp_is_main(rp: *const u8) -> bool {
    let width =
        unsafe { std::ptr::read_unaligned(rp.add(offsets::RP_WIDTH) as *const u16) } as u32;
    let base_w = BASE_W.load(Ordering::Relaxed);
    base_w == 0 || width * 2 >= base_w
}

// wraps the game building the world RenderParameters. the boost itself is
// applied inside (by the centerOn hook); this hook gates it to the main
// view and publishes the resulting rect
fn hooked_create_rp(
    this: *mut core::ffi::c_void,
    ret: *mut core::ffi::c_void,
) -> *mut core::ffi::c_void {
    RP_BOOST.with(|b| b.set(1.0));
    MAIN_RP_ACTIVE.with(|f| f.set(true));
    let out = unsafe { CreateRpHook.call(this, ret) };
    MAIN_RP_ACTIVE.with(|f| f.set(false));

    if ret.is_null() || !rp_is_main(ret as *const u8) {
        return out;
    }

    let boost = RP_BOOST.with(|b| b.get());
    if boost > 1.001 && !BOOST_LOGGED.swap(true, Ordering::Relaxed) {
        log::info!("[frame] horizon boost active: x{boost:.2}");
    }
    ZOOM_BOOST.set(boost);

    // publish the final view rect (tiles)
    if !ret.is_null() {
        unsafe {
            let rect = (ret as *const u8).add(offsets::RP_RECT) as *const i32;
            let (l, t, r, b) = (*rect, *rect.add(1), *rect.add(2), *rect.add(3));
            let span_x = (r - l) as f32 / offsets::RECT_FP;
            let span_y = (b - t) as f32 / offsets::RECT_FP;
            if span_x > 0.1 && span_y > 0.1 {
                RECT_L.set(l as f32 / offsets::RECT_FP);
                RECT_T.set(t as f32 / offsets::RECT_FP);
                SPAN_X.set(span_x);
                SPAN_Y.set(span_y);
            }
        }
    }
    out
}

// game-initiated resize: remember the fresh vanilla size
fn hooked_ack_resize(this: *mut core::ffi::c_void, width: u16, height: u16) {
    set_base_size(width as u32, height as u32);
    unsafe { AckResizeHook.call(this, width, height) }
}

// --- the horizon zoom boost ----------------------------------------------------------
// centerOn reads the zoom out of the SurfaceView and derives everything from
// it. dividing that zoom around the call (edit -> call -> restore) renders
// more world with every derived field consistent. the zoom's offset inside
// SurfaceView is unknown, so it self-calibrates on the first call.

// candidate byte offsets of the zoom f64 inside SurfaceView (front = active)
static SV_ZOOM_CANDS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
// 0 = calibrating, 1 = calibrated, 2 = failed (boost off this session)
static SV_CALIB_STATE: AtomicU32 = AtomicU32::new(0);
static SV_CALIB_TRIES: AtomicU64 = AtomicU64::new(0);
// give up calibrating after this many frames without a zoom-offset match
const MAX_CALIB_TRIES: u64 = 300;

fn hooked_center_on(
    this: *mut core::ffi::c_void,
    map: *const core::ffi::c_void,
    surface_view: *const core::ffi::c_void,
    r9: usize,
) -> usize {
    let boosting = settings::HORIZON_BOOST
        && MAIN_RP_ACTIVE.with(|f| f.get())
        && !this.is_null()
        && !surface_view.is_null();
    if !boosting {
        return unsafe { CenterOnHook.call(this, map, surface_view, r9) };
    }
    // main view only: preview views must never be boosted or calibrated against
    if !rp_is_main(this as *const u8) {
        return unsafe { CenterOnHook.call(this, map, surface_view, r9) };
    }

    match SV_CALIB_STATE.load(Ordering::Relaxed) {
        0 => calibrate_zoom_offset(this, map, surface_view, r9),
        1 => boosted_center_on(this, map, surface_view, r9),
        _ => unsafe { CenterOnHook.call(this, map, surface_view, r9) },
    }
}

// calibration (unboosted): snapshot the first 16 f64s of the SurfaceView,
// run centerOn, keep the offsets equal to the resulting scale — that's
// where the zoom lives
fn calibrate_zoom_offset(
    this: *mut core::ffi::c_void,
    map: *const core::ffi::c_void,
    surface_view: *const core::ffi::c_void,
    r9: usize,
) -> usize {
    unsafe {
        let mut snap = [0.0f64; 16];
        for (i, s) in snap.iter_mut().enumerate() {
            *s = std::ptr::read_unaligned((surface_view as *const u8).add(i * 8) as *const f64);
        }
        let ret = CenterOnHook.call(this, map, surface_view, r9);
        let scale = *((this as *const u8).add(offsets::RP_SCALE) as *const f64);
        if scale.is_finite() && scale > 1e-6 {
            let matches: Vec<usize> = (0..16)
                .filter(|&i| (snap[i] - scale).abs() < 1e-9 * scale.max(1.0))
                .map(|i| i * 8)
                .collect();
            if !matches.is_empty() {
                log::info!("[frame] zoom offset calibrated: {matches:?} (scale {scale:.4})");
                if let Ok(mut c) = SV_ZOOM_CANDS.lock() {
                    *c = matches;
                }
                SV_CALIB_STATE.store(1, Ordering::Relaxed);
            } else if SV_CALIB_TRIES.fetch_add(1, Ordering::Relaxed) > MAX_CALIB_TRIES {
                log::warn!(
                    "[frame] zoom calibration failed (display scale not 100%?) — horizon boost disabled"
                );
                SV_CALIB_STATE.store(2, Ordering::Relaxed);
            }
        }
        ret
    }
}

// calibrated: divide the zoom around the centerOn call so the game renders
// more world, then verify the edit really drove the output scale (a wrong
// candidate offset is dropped and the next one tried next frame)
fn boosted_center_on(
    this: *mut core::ffi::c_void,
    map: *const core::ffi::c_void,
    surface_view: *const core::ffi::c_void,
    r9: usize,
) -> usize {
    unsafe {
        let off = SV_ZOOM_CANDS.lock().ok().and_then(|c| c.first().copied());
        let Some(off) = off else {
            SV_CALIB_STATE.store(2, Ordering::Relaxed);
            return CenterOnHook.call(this, map, surface_view, r9);
        };
        let zoom_ptr = (surface_view as *const u8).add(off) as *mut f64;
        let zoom = std::ptr::read_unaligned(zoom_ptr);
        let w_px = *((this as *const u8).add(offsets::RP_WIDTH) as *const u16) as f64;

        // boost factor: aim for TARGET_SPAN_TILES whenever the 3d view is
        // engaged, within the caps. exact top-down stays pure vanilla
        let mut boost = 1.0f64;
        if zoom.is_finite() && zoom > 1e-6 && w_px > 16.0 {
            let span = w_px / (zoom * offsets::PX_PER_TILE_SCALE_1);
            let (yaw, pitch, _z) = crate::camera::get();
            let engaged = pitch < 89.0 || yaw.abs() > 1.0;
            if engaged {
                boost = (settings::TARGET_SPAN_TILES / span)
                    .max(1.0)
                    .min(settings::MAX_ZOOM_BOOST)
                    .min((settings::MAX_BOOST_SPAN_TILES / span).max(1.0))
                    .min((zoom / settings::MIN_EFFECTIVE_SCALE).max(1.0));
            }
        }
        if boost <= 1.001 {
            return CenterOnHook.call(this, map, surface_view, r9);
        }

        std::ptr::write_unaligned(zoom_ptr, zoom / boost);
        let ret = CenterOnHook.call(this, map, surface_view, r9);
        std::ptr::write_unaligned(zoom_ptr, zoom);

        let scale = *((this as *const u8).add(offsets::RP_SCALE) as *const f64);
        let expected = zoom / boost;
        if (scale - expected).abs() < 1e-6 * expected.max(1e-9) {
            RP_BOOST.with(|b| b.set(boost as f32));
        } else if let Ok(mut c) = SV_ZOOM_CANDS.lock() {
            if !c.is_empty() {
                c.remove(0);
            }
            log::warn!(
                "[frame] zoom offset 0x{off:X} didn't drive the scale — {} candidate(s) left",
                c.len()
            );
            if c.is_empty() {
                SV_CALIB_STATE.store(2, Ordering::Relaxed);
            }
        }
        ret
    }
}

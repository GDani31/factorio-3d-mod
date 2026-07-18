// per-frame orchestration: right after the game finishes rendering the
// world fbo, warp it in place (when the camera is engaged) and draw the 3d
// model instances on top. factorio then composites the fbo and draws the
// hud flat over it — gui/menu separation is automatic.

use crate::camera;
use crate::model_renderer::ModelRenderer;
use crate::settings;
use crate::warp::WarpPipeline;
use anyhow::Result;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::core::Interface;

static WARP_ENGAGED_LOGGED: AtomicBool = AtomicBool::new(false);

struct DxState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    warp: WarpPipeline,
    models: Option<ModelRenderer>,
}

pub struct Renderer3D {
    frame_count: AtomicU64,
    dx_state: Mutex<Option<DxState>>,
    dx_initialized: AtomicBool,
    last_time: Mutex<Option<Instant>>,
}

impl Renderer3D {
    pub fn new() -> Result<Self> {
        Ok(Self {
            frame_count: AtomicU64::new(0),
            dx_state: Mutex::new(None),
            dx_initialized: AtomicBool::new(false),
            last_time: Mutex::new(None),
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
        let models = match ModelRenderer::new(&device) {
            Ok(r) => Some(r),
            Err(e) => {
                log::error!("[renderer] model pipeline failed: {e:#} — models disabled");
                None
            }
        };

        *self.dx_state.lock().unwrap() = Some(DxState { device, context, warp, models });
        Ok(())
    }

    // called right after GameRenderer::render — the world fbo is still bound
    pub fn on_after_world_render(&self) {
        if !self.dx_initialized.load(Ordering::Relaxed) {
            return;
        }
        crate::tuning::poll(crate::hooks::frame::frame_count());
        let (yaw, pitch, zoom) = camera::get();

        let mut state_guard = self.dx_state.lock().unwrap();
        let state = match state_guard.as_mut() {
            Some(s) => s,
            None => return,
        };
        let DxState { device, context, warp, models } = state;

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
            let aspect = fbo_desc.Width as f32 / fbo_desc.Height.max(1) as f32;

            let plane_scale = crate::hooks::frame::zoom_boost_applied().max(1.0);
            let fps = camera::fps_mode();
            let rotated = fps
                || yaw.abs() > 1.0
                || (pitch - 90.0).abs() > 1.0
                || (zoom - 1.0).abs() > 0.05;

            // first-person eye height: 1.7 tiles in plane units
            let (_, _, _, span_y) = crate::hooks::frame::view_rect_tiles();
            let fps_eye_h =
                if span_y > 0.5 { 1.7 * 2.0 * plane_scale / span_y } else { 0.05 };

            // at yaw 0 / pitch 90 / zoom 1 this is exactly the vanilla view,
            // so the same matrix serves the warp AND the flat model pass
            let view_proj =
                crate::warp::build_view_proj(yaw, pitch, aspect, zoom, plane_scale, fps_eye_h);

            if rotated {
                if !WARP_ENGAGED_LOGGED.swap(true, Ordering::Relaxed) {
                    log::info!(
                        "[renderer] warp engaged (yaw={yaw:.1} pitch={pitch:.1} zoom={zoom:.2})"
                    );
                }
                warp.warp_in_place(device, context, rtv, &world_tex, &view_proj, plane_scale);
                let (mut win_w, mut win_h) = crate::hooks::frame::base_size();
                if win_w == 0 || win_h == 0 {
                    (win_w, win_h) = (fbo_desc.Width, fbo_desc.Height);
                }
                crate::picking::set_transform(&view_proj, aspect, win_w, win_h);
            } else {
                crate::picking::clear();
            }

            // 3d model instances (drawn in flat view too — the vanilla sprite
            // is suppressed by the draw hook either way)
            if let Some(mr) = models.as_mut() {
                let dt = {
                    let mut last = self.last_time.lock().unwrap();
                    let now = Instant::now();
                    let dt = last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
                    *last = Some(now);
                    dt.clamp(0.0, 0.1)
                };
                let view_rect = crate::hooks::frame::view_rect_tiles();
                let (_, _, csx, _) = view_rect;
                // skip absurd spans (map view etc.)
                if csx > 1.0 && (csx as f64) < settings::MAX_BOOST_SPAN_TILES * 2.0 {
                    let instances = crate::entities::tick(dt, view_rect);
                    let wires = crate::entities::take_wires();
                    if !instances.is_empty() || !wires.is_empty() {
                        let cam_eye =
                            crate::warp::camera_eye(yaw, pitch, zoom, plane_scale, fps_eye_h);
                        mr.draw(
                            device,
                            context,
                            rtv,
                            fbo_desc.Width,
                            fbo_desc.Height,
                            &view_proj,
                            cam_eye,
                            plane_scale,
                            view_rect,
                            &instances,
                            &wires,
                        );
                    }
                }
            }
        }
    }
}

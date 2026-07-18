// the 3d warp, minimal edition: treats the rendered frame as a flat textured
// plane and re-renders it from an orbiting camera. no billboards, no lifted
// layers — just the tilted ground with a plain sky behind it.

use crate::settings::LOOK_AHEAD;
use anyhow::{Context, Result};
use glam::{Mat4, Vec3, Vec4};
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

// constant buffer shared by the warp shaders (must match CB_HLSL below)
#[repr(C)]
#[derive(Clone, Copy)]
struct CameraCB {
    view_proj: [f32; 16],
    aspect: f32,
    plane_scale: f32,
    tex_w: f32,
    tex_h: f32,
    sky: [f32; 4], // x = night factor (sky gradient day/dusk/night)
}

// hlsl lives in src/shaders/; the cbuffer block is prepended to each stage
const CB_HLSL: &str = include_str!("shaders/warp_common.hlsl");
const PLANE_VS: &str = include_str!("shaders/warp_vs.hlsl");
const PLANE_PS: &str = include_str!("shaders/warp_ps.hlsl");
const SKY_VS: &str = include_str!("shaders/sky_vs.hlsl");
const SKY_PS: &str = include_str!("shaders/sky_ps.hlsl");

pub struct WarpPipeline {
    staging_tex: ID3D11Texture2D,
    staging_srv: ID3D11ShaderResourceView,
    staging_w: u32,
    staging_h: u32,
    staging_format: DXGI_FORMAT,
    plane_vs: ID3D11VertexShader,
    plane_ps: ID3D11PixelShader,
    sky_vs: ID3D11VertexShader,
    sky_ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    no_cull_rs: ID3D11RasterizerState,
}

impl WarpPipeline {
    pub fn new(device: &ID3D11Device, back_buffer: &ID3D11Texture2D) -> Result<Self> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { back_buffer.GetDesc(&mut desc) };
        log::info!("[warp] back buffer {}x{}", desc.Width, desc.Height);

        let (staging_tex, staging_srv) =
            create_staging(device, desc.Width, desc.Height, desc.Format)?;

        let plane_vs = create_vs(device, &format!("{CB_HLSL}{PLANE_VS}"))?;
        let plane_ps = create_ps(device, &format!("{CB_HLSL}{PLANE_PS}"))?;
        let sky_vs = create_vs(device, &format!("{CB_HLSL}{SKY_VS}"))?;
        let sky_ps = create_ps(device, &format!("{CB_HLSL}{SKY_PS}"))?;

        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<CameraCB>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let cb = unsafe {
            let mut buf = None;
            device.CreateBuffer(&cb_desc, None, Some(&mut buf))?;
            buf.context("CreateBuffer CB")?
        };

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ..Default::default()
        };
        let sampler = unsafe {
            let mut s = None;
            device.CreateSamplerState(&sampler_desc, Some(&mut s))?;
            s.context("CreateSamplerState")?
        };

        let no_cull_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let no_cull_rs = unsafe {
            let mut rs = None;
            device.CreateRasterizerState(&no_cull_desc, Some(&mut rs))?;
            rs.context("CreateRasterizerState")?
        };

        log::info!("[warp] pipeline created");
        Ok(Self {
            staging_tex,
            staging_srv,
            staging_w: desc.Width,
            staging_h: desc.Height,
            staging_format: desc.Format,
            plane_vs,
            plane_ps,
            sky_vs,
            sky_ps,
            cb,
            sampler,
            no_cull_rs,
        })
    }

    // (re)create the staging texture when the fbo size changes
    fn ensure_staging(
        &mut self,
        device: &ID3D11Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
    ) -> bool {
        if self.staging_w == w && self.staging_h == h && self.staging_format == format {
            return true;
        }
        match create_staging(device, w, h, format) {
            Ok((tex, srv)) => {
                self.staging_tex = tex;
                self.staging_srv = srv;
                self.staging_w = w;
                self.staging_h = h;
                self.staging_format = format;
                true
            }
            Err(_) => false,
        }
    }

    // warp the world fbo in place: copy it to staging, clear it to the sky
    // color, then draw the ground plane back onto it from the tilted camera
    pub fn warp_in_place(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        texture: &ID3D11Texture2D,
        view_proj: &Mat4,
        plane_scale: f32,
    ) {
        let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut tex_desc) };
        if !self.ensure_staging(device, tex_desc.Width, tex_desc.Height, tex_desc.Format) {
            return;
        }
        let (tex_w, tex_h) = (tex_desc.Width as f32, tex_desc.Height as f32);
        let aspect = tex_w / tex_h;

        unsafe {
            context.CopyResource(&self.staging_tex, texture);

            // save the game's state; restore at the end
            let mut prev_rtvs = [None; 1];
            let mut prev_dsv = None;
            context.OMGetRenderTargets(Some(&mut prev_rtvs), Some(&mut prev_dsv));
            let mut prev_vps = [D3D11_VIEWPORT::default(); 1];
            let mut num_vp = 1u32;
            context.RSGetViewports(&mut num_vp, Some(prev_vps.as_mut_ptr()));
            let prev_rs: Option<ID3D11RasterizerState> = context.RSGetState().ok();
            context.RSSetState(&self.no_cull_rs);

            let mut data = CameraCB {
                view_proj: [0.0; 16],
                aspect,
                plane_scale,
                tex_w,
                tex_h,
                sky: [crate::hooks::daynight::night_factor(), 0.0, 0.0, 0.0],
            };
            data.view_proj.copy_from_slice(view_proj.as_ref());
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if context.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).is_ok() {
                std::ptr::copy_nonoverlapping(
                    &data as *const CameraCB as *const u8,
                    mapped.pData as *mut u8,
                    std::mem::size_of::<CameraCB>(),
                );
                context.Unmap(&self.cb, 0);
            }

            let sky = crate::settings::SKY_COLOR;
            context.ClearRenderTargetView(rtv, &[sky[0], sky[1], sky[2], 1.0]);
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: tex_w,
                Height: tex_h,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            context.RSSetViewports(Some(&[viewport]));
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.IASetInputLayout(None);
            // sky first: fullscreen day/dusk/night gradient behind the plane
            context.VSSetShader(&self.sky_vs, None);
            context.PSSetShader(&self.sky_ps, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.Draw(4, 0);
            context.VSSetShader(&self.plane_vs, None);
            context.PSSetShader(&self.plane_ps, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(self.staging_srv.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(4, 0);

            // restore state
            let null_srvs: [Option<ID3D11ShaderResourceView>; 1] = [None];
            context.PSSetShaderResources(0, Some(&null_srvs));
            context.OMSetRenderTargets(Some(&prev_rtvs), prev_dsv.as_ref());
            if num_vp > 0 {
                context.RSSetViewports(Some(&prev_vps[..num_vp as usize]));
            }
            context.RSSetState(prev_rs.as_ref());
        }
    }
}

// camera that tilts (pitch) and orbits (yaw) around the player at the plane
// origin. at yaw 0 / pitch 90 / zoom 1 the mapping is exactly the vanilla
// view, so the model pass can use it even when the warp is off. first person
// places the eye at the player (plane origin) at head height instead
pub fn build_view_proj(
    yaw: f32,
    pitch: f32,
    aspect: f32,
    zoom: f32,
    plane_scale: f32,
    fps_eye_h: f32,
) -> Mat4 {
    if crate::camera::fps_mode() {
        // yaw 0 faces north; pitch ~25 = horizon, higher = looking down
        let az = yaw.to_radians();
        let down = (pitch - 25.0).to_radians();
        let (sd, cd) = down.sin_cos();
        let (sa, ca) = az.sin_cos();
        let fwd = Vec3::new(-cd * sa, -sd, cd * ca);
        let eye = Vec3::new(0.0, fps_eye_h.max(0.01), 0.0);
        let view = Mat4::look_at_lh(eye, eye + fwd, Vec3::Y);
        let proj = Mat4::perspective_lh(75.0f32.to_radians(), aspect, 0.005, 100.0);
        return proj * view;
    }
    let (se, ce) = pitch.to_radians().sin_cos(); // 90 = straight above (top-down)
    let (sa, ca) = yaw.to_radians().sin_cos();
    let eye = orbit_eye(yaw, pitch, zoom, plane_scale);
    // up = the elevation tangent, which keeps yaw working even at exact top-down
    let up = Vec3::new(-se * sa, ce, se * ca);
    let view = Mat4::look_at_lh(eye, Vec3::ZERO, up);
    let proj = Mat4::perspective_lh(ORBIT_FOV_DEG.to_radians(), aspect, 0.01, 100.0);

    // drop the pivot toward the lower foreground when tilted, so rotation
    // circles the player instead of the horizon (a clip-space shift — aiming
    // the camera instead broke the orbit symmetry)
    let fg = LOOK_AHEAD * ce;
    let shift = Mat4::from_cols(
        Vec4::new(1.0, 0.0, 0.0, 0.0),
        Vec4::new(0.0, 1.0, 0.0, 0.0),
        Vec4::new(0.0, 0.0, 1.0, 0.0),
        Vec4::new(0.0, -fg, 0.0, 1.0),
    );
    shift * proj * view
}

const ORBIT_FOV_DEG: f32 = 60.0;

// eye position of the orbit camera, shared by build_view_proj and camera_eye
// so the specular view direction can't drift from the real view
fn orbit_eye(yaw: f32, pitch: f32, zoom: f32, plane_scale: f32) -> Vec3 {
    let base_dist = 1.0 / (ORBIT_FOV_DEG.to_radians() / 2.0).tan();
    let mut dist = base_dist * zoom;
    // keep the camera above the finite plane (its horizontal reach is
    // cos(el)*dist; the plane edge is at plane_scale)
    let (se, ce) = pitch.to_radians().sin_cos();
    if ce > 0.01 {
        dist = dist.min(plane_scale * 0.95 / ce);
    }
    let (sa, ca) = yaw.to_radians().sin_cos();
    Vec3::new(ce * sa, se, -ce * ca) * dist
}

// world-space (plane-space) camera position for the same view build_view_proj
// makes. the model shader needs it for specular highlights (view direction).
// the pivot shift above is a clip-space nudge, so it doesn't move the eye
pub fn camera_eye(yaw: f32, pitch: f32, zoom: f32, plane_scale: f32, fps_eye_h: f32) -> Vec3 {
    if crate::camera::fps_mode() {
        return Vec3::new(0.0, fps_eye_h.max(0.01), 0.0);
    }
    orbit_eye(yaw, pitch, zoom, plane_scale)
}

fn create_staging(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w.max(1),
        Height: h.max(1),
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let tex = unsafe {
        let mut tex = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        tex.context("CreateTexture2D staging")?
    };
    let srv = unsafe {
        let mut srv = None;
        device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        srv.context("CreateSRV staging")?
    };
    Ok((tex, srv))
}

pub(crate) fn create_vs(device: &ID3D11Device, source: &str) -> Result<ID3D11VertexShader> {
    let bytecode = compile_shader(source, "vs_5_0")?;
    unsafe {
        let mut out = None;
        device.CreateVertexShader(&bytecode, None, Some(&mut out))?;
        out.context("CreateVertexShader")
    }
}

pub(crate) fn create_ps(device: &ID3D11Device, source: &str) -> Result<ID3D11PixelShader> {
    let bytecode = compile_shader(source, "ps_5_0")?;
    unsafe {
        let mut out = None;
        device.CreatePixelShader(&bytecode, None, Some(&mut out))?;
        out.context("CreatePixelShader")
    }
}

pub(crate) fn compile_shader(source: &str, target: &str) -> Result<Vec<u8>> {
    let source_bytes = source.as_bytes();
    let entry = std::ffi::CString::new("main")?;
    let target = std::ffi::CString::new(target)?;
    unsafe {
        let mut blob = None;
        let mut error_blob = None;
        let hr = D3DCompile(
            source_bytes.as_ptr() as *const _,
            source_bytes.len(),
            None,
            None,
            None,
            windows::core::PCSTR(entry.as_ptr() as *const u8),
            windows::core::PCSTR(target.as_ptr() as *const u8),
            0,
            0,
            &mut blob,
            Some(&mut error_blob),
        );
        if let Some(err) = error_blob {
            let msg = std::slice::from_raw_parts(
                err.GetBufferPointer() as *const u8,
                err.GetBufferSize(),
            );
            log::error!("[warp] shader compile error: {}", String::from_utf8_lossy(msg));
        }
        hr.context("D3DCompile failed")?;
        let blob = blob.context("no shader blob")?;
        let data =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        Ok(data.to_vec())
    }
}

// the 3d warp: treats the rendered frame as a flat textured plane and
// re-renders it from an orbiting camera, then draws the captured layers on
// top — objects as standing billboards, belts/rails/wires as lifted planes.

use crate::capture::{HI_SUPER, MAX_HI_SLICES};
use crate::settings::{LOOK_AHEAD, MAX_BILLBOARDS, STAND_SCALE};
use anyhow::{Context, Result};
use glam::{Mat4, Vec3, Vec4};
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

// one standing billboard quad, in pre-warp texture coordinates
#[derive(Clone, Copy)]
pub struct BillboardUv {
    pub u0: f32,
    pub u1: f32,
    pub v_top: f32,
    pub v_base: f32,
    // entity feet in texture-v: the quad's ground anchor
    pub v_foot: f32,
    // the ONE hi tile this quad samples (-1 = low-res only). one tile per
    // quad keeps sprites temporally coherent (tiles render on different frames)
    pub sel: f32,
    // position-u of the quad center (vehicles anchor at their world position)
    pub pu: f32,
    // lay flat on the ground instead of standing up
    pub flat: bool,
    // extra height above the ground in plane units (flying robots; 0 = normal)
    pub fly_lift: f32,
}

// textures captured this frame, passed to the warp
pub struct WarpLayers<'a> {
    pub object: Option<&'a ID3D11ShaderResourceView>,
    pub object_hi: Option<&'a ID3D11ShaderResourceView>,
    pub ground_hi: Option<&'a ID3D11ShaderResourceView>,
    pub belt: Option<&'a ID3D11ShaderResourceView>,
    pub elevated: Option<&'a ID3D11ShaderResourceView>,
    pub wire: Option<&'a ID3D11ShaderResourceView>,
}

// everything else the warp needs this frame
pub struct WarpParams<'a> {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
    // total world-extension factor (the zoom boost): the flat camera frames
    // only the plane center 1/plane_scale; tilting reveals the extra world
    pub plane_scale: f32,
    pub belt_lift: f32,
    pub elevated_lift: f32,
    pub wire_lift: f32,
    // first-person eye height in plane units
    pub fps_eye_h: f32,
    pub hi_grid: f32,
    // per hi tile: current-uv -> stamp-uv affine (ax, bx, ay, by)
    pub tile_affine: &'a [[f32; 4]; MAX_HI_SLICES],
    // per hi tile: view coverage per axis (0 = tile invalid)
    pub tile_cover: &'a [f32; MAX_HI_SLICES],
}

// constant buffer shared by all warp shaders (must match CB_HLSL below)
#[repr(C)]
#[derive(Clone, Copy)]
struct CameraCB {
    view_proj: [f32; 16],
    aspect: f32,
    plane_scale: f32,
    tex_w: f32,
    tex_h: f32,
    lift_y: f32,
    cam_u: f32,
    cam_v: f32,
    tint: f32,
    hi_on: f32,
    hi_tex_w: f32,
    hi_tex_h: f32,
    hi_grid: f32,
    tex_v_shift: f32,
    _pad: [f32; 3],
    tile_affine: [[f32; 4]; MAX_HI_SLICES],
    tile_cover: [f32; 4],
    tile_cover_live: f32,
    _pad2: [f32; 3],
}

const CB_HLSL: &str = r#"
cbuffer CameraCB : register(b0) {
    float4x4 viewProj;
    float aspect;       // fbo width / height
    float planeScale;   // world-extension factor (zoom boost)
    float texW;         // fbo pixel size (0 = plain passthrough sampling)
    float texH;
    float liftY;        // plane height above ground (belt/rail passes)
    float camU;         // camera position in the texture (0.5 = center)
    float camV;
    float tint;         // brightness multiplier (belt edge slices)
    float hiOn;         // 1 = hi-res tile compositing enabled
    float hiTexW;       // hi tile pixel size (HI_SUPER x fbo)
    float hiTexH;
    float hiGrid;       // hi tiling grid per axis
    float texVShift;    // added to uv.y before sampling (wire plane realign)
    float3 _pad;
    float4 tileAffine[5]; // per tile: current-uv -> stamp-uv (ax,bx,ay,by); [4] = live
    float4 tileCoverA;    // grid tiles 0..3: view coverage (0 = invalid)
    float tileCoverLive;  // live tile (slice 4) coverage
    float3 _pad2;
};
"#;

// plane vertex shader: maps the whole fbo (uv 0..1) onto a ground plane
// extended by planeScale, with the camera's texture point at the origin
const PLANE_VS: &str = r#"
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

VS_OUT main(uint id : SV_VertexID) {
    float2 uv = float2(id & 1, id >> 1);
    float3 worldPos = float3(
        (uv.x - camU) * 2.0 * aspect * planeScale,
        liftY,
        (camV - uv.y) * 2.0 * planeScale
    );
    VS_OUT o;
    o.pos = mul(viewProj, float4(worldPos, 1.0));
    o.uv = uv;
    return o;
}
"#;

// plane pixel shader with sharp-bilinear sampling (plain bilinear turns the
// magnified near field into mush) and optional hi-res tile compositing
const PLANE_PS: &str = r#"
Texture2D sceneTex      : register(t0);
Texture2DArray hiTex    : register(t1);
SamplerState samp       : register(s0);

float4 sharpTap(Texture2D tex, float2 uv, float2 ts, float sharpen) {
    float2 p = uv * ts;
    float2 tf = floor(p);
    float2 cd = (p - tf) - 0.5;
    float2 rr = 0.5 - 0.5 / sharpen;
    float2 f = (cd - clamp(cd, -rr, rr)) * sharpen + 0.5;
    return tex.Sample(samp, (tf + f) / ts);
}

float4 sharpTapA(Texture2DArray tex, float2 uv, float slice, float2 ts, float sharpen) {
    float2 p = uv * ts;
    float2 tf = floor(p);
    float2 cd = (p - tf) - 0.5;
    float2 rr = 0.5 - 0.5 / sharpen;
    float2 f = (cd - clamp(cd, -rr, rr)) * sharpen + 0.5;
    return tex.Sample(samp, float3((tf + f) / ts, slice));
}

// hi-res temporal tile lookup: view uv -> grid cell -> that tile's slice,
// remapped through the tile's own stamp (tiles render on different frames)
float4 hiSample(float2 uv, float sharpen, out float valid) {
    float g = max(hiGrid, 1.0);
    float2 tc = clamp(floor(uv * g), 0.0, g - 1.0);
    int ti = int(tc.y * g + tc.x);
    float4 A = tileAffine[ti];
    float cov = tileCoverA[ti];
    float2 uvt = float2(uv.x * A.x + A.y, uv.y * A.z + A.w);
    float2 c = (tc + 0.5) / g;
    float2 d = uvt - c;
    float lim = 0.5 * cov;
    valid = (cov > 0.01 && abs(d.x) <= lim && abs(d.y) <= lim) ? 1.0 : 0.0;
    float2 huv = clamp(0.5 + d / max(cov, 0.01), 0.0005, 0.9995);
    return sharpTapA(hiTex, huv, float(ti), float2(hiTexW, hiTexH), sharpen);
}

float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    uv.y += texVShift;
    float t = (tint > 0.01) ? tint : 1.0;
    if (texW < 1.0) {
        float4 c0 = sceneTex.Sample(samp, uv);
        c0.rgb *= t;
        return c0;
    }
    float sharp = max(planeScale * 4.0, 6.0);
    float4 c = sharpTap(sceneTex, uv, float2(texW, texH), sharp);
    if (hiOn > 0.5) {
        float valid;
        float4 hc = hiSample(uv, sharp, valid);
        if (valid > 0.5)
            c = hc + (1.0 - hc.a) * c;
    }
    c.rgb *= t;
    return c;
}
"#;

// billboard vertex shader: quads are built cpu-side, this just projects
const ENTITY_VS: &str = r#"
struct VS_IN {
    float3 pos : POSITION;
    float2 uv  : TEXCOORD0;
    float  sel : TEXCOORD1;
    float2 foot : TEXCOORD2;
};
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
    float  sel : TEXCOORD1;
    float2 foot : TEXCOORD2;
};

VS_OUT main(VS_IN i) {
    VS_OUT o;
    o.pos = mul(viewProj, float4(i.pos, 1.0));
    o.uv = i.uv;
    o.sel = i.sel;
    o.foot = i.foot;
    return o;
}
"#;

// billboard pixel shader: t0 = low-res object capture, t1 = hi tile array,
// t2 = warped ground plane (billboards on black/unrevealed ground vanish
// with it instead of floating in the void)
const ENTITY_PS: &str = r#"
Texture2D sceneTex      : register(t0);
Texture2DArray hiTex    : register(t1);
Texture2D groundTex     : register(t2);
SamplerState samp       : register(s0);

float4 sharpSample(Texture2D tex, float2 uv, float2 ts, float sharpen) {
    float2 p = uv * ts;
    float2 tf = floor(p);
    float2 cd = (p - tf) - 0.5;
    float2 rr = 0.5 - 0.5 / sharpen;
    float2 f = (cd - clamp(cd, -rr, rr)) * sharpen + 0.5;
    return tex.Sample(samp, (tf + f) / ts);
}

float4 sharpSampleA(Texture2DArray tex, float2 uv, float slice, float2 ts, float sharpen) {
    float2 p = uv * ts;
    float2 tf = floor(p);
    float2 cd = (p - tf) - 0.5;
    float2 rr = 0.5 - 0.5 / sharpen;
    float2 f = (cd - clamp(cd, -rr, rr)) * sharpen + 0.5;
    return tex.Sample(samp, float3((tf + f) / ts, slice));
}

float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0, float sel : TEXCOORD1,
            float2 foot : TEXCOORD2) : SV_TARGET {
    float4 g = groundTex.Sample(samp, foot);
    if (g.r + g.g + g.b < 0.02)
        return float4(0, 0, 0, 0);
    float sharp = max(planeScale * 4.0, 6.0);
    if (sel > -0.5 && hiOn > 0.5) {
        int gi = int(max(hiGrid, 1.0) + 0.5);
        int ti = int(sel + 0.5);
        float4 A = tileAffine[ti];
        float cov = (ti < 4) ? tileCoverA[ti] : tileCoverLive;
        float2 uvt = float2(uv.x * A.x + A.y, uv.y * A.z + A.w);
        float2 tc = float2(ti % gi, ti / gi);
        float2 c = (ti < 4) ? ((tc + 0.5) / float(gi)) : float2(0.5, 0.5);
        float2 d = uvt - c;
        float lim = 0.5 * cov;
        if (cov > 0.01 && abs(d.x) <= lim && abs(d.y) <= lim) {
            float2 huv = clamp(0.5 + d / cov, 0.0005, 0.9995);
            float4 h = sharpSampleA(hiTex, huv, float(ti), float2(hiTexW, hiTexH), sharp);
            if (h.a > 0.004)
                return h;
            // hi tile empty here — fall back to low-res below
        }
    }
    return sharpSample(sceneTex, uv, float2(texW, texH), sharp);
}
"#;

// fullscreen passthrough vertex shader (flat compositing)
const FLAT_VS: &str = r#"
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

VS_OUT main(uint id : SV_VertexID) {
    float2 uv = float2(id & 1, id >> 1);
    float2 ndc = uv * 2.0 - 1.0;
    ndc.y = -ndc.y;
    VS_OUT o;
    o.pos = float4(ndc, 0, 1);
    o.uv = uv;
    return o;
}
"#;

pub struct WarpPipeline {
    staging_tex: ID3D11Texture2D,
    staging_srv: ID3D11ShaderResourceView,
    staging_w: u32,
    staging_h: u32,
    staging_format: DXGI_FORMAT,
    plane_vs: ID3D11VertexShader,
    plane_ps: ID3D11PixelShader,
    entity_vs: ID3D11VertexShader,
    entity_ps: ID3D11PixelShader,
    entity_layout: ID3D11InputLayout,
    entity_vb: ID3D11Buffer,
    flat_vs: ID3D11VertexShader,
    // factorio renders premultiplied-alpha sprites; captures composite with ONE
    premult_blend: ID3D11BlendState,
    cb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    // standing quads face away at some yaws — never cull them
    no_cull_rs: ID3D11RasterizerState,
    // vanilla window size (back buffer at init)
    win_w: u32,
    win_h: u32,
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
        let flat_vs = create_vs(device, FLAT_VS)?;

        // billboard pipeline: real vertex stream (8 floats = 32-byte stride)
        let entity_bytecode = compile_shader(&format!("{CB_HLSL}{ENTITY_VS}"), "vs_5_0")?;
        let entity_vs = unsafe {
            let mut out = None;
            device.CreateVertexShader(&entity_bytecode, None, Some(&mut out))?;
            out.context("CreateVertexShader entity")?
        };
        let layout_desc = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("POSITION"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("TEXCOORD"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 12,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("TEXCOORD"),
                SemanticIndex: 1,
                Format: DXGI_FORMAT_R32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 20,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: windows::core::s!("TEXCOORD"),
                SemanticIndex: 2,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 24,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let entity_layout = unsafe {
            let mut lo = None;
            device.CreateInputLayout(&layout_desc, &entity_bytecode, Some(&mut lo))?;
            lo.context("CreateInputLayout entity")?
        };
        let entity_vb_desc = D3D11_BUFFER_DESC {
            ByteWidth: (MAX_BILLBOARDS * 6 * 8 * 4) as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let entity_vb = unsafe {
            let mut buf = None;
            device.CreateBuffer(&entity_vb_desc, None, Some(&mut buf))?;
            buf.context("CreateBuffer entity VB")?
        };
        let entity_ps = create_ps(device, &format!("{CB_HLSL}{ENTITY_PS}"))?;

        let premult_blend_desc = D3D11_BLEND_DESC {
            RenderTarget: {
                let mut rt = [D3D11_RENDER_TARGET_BLEND_DESC::default(); 8];
                rt[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                    BlendEnable: true.into(),
                    SrcBlend: D3D11_BLEND_ONE,
                    DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                    BlendOp: D3D11_BLEND_OP_ADD,
                    SrcBlendAlpha: D3D11_BLEND_ONE,
                    DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                    BlendOpAlpha: D3D11_BLEND_OP_ADD,
                    RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                };
                rt
            },
            ..Default::default()
        };
        let premult_blend = unsafe {
            let mut bs = None;
            device.CreateBlendState(&premult_blend_desc, Some(&mut bs))?;
            bs.context("CreateBlendState")?
        };

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
            entity_vs,
            entity_ps,
            entity_layout,
            entity_vb,
            flat_vs,
            premult_blend,
            cb,
            sampler,
            no_cull_rs,
            win_w: desc.Width,
            win_h: desc.Height,
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

    fn write_cb(
        &self,
        context: &ID3D11DeviceContext,
        view_proj: &Mat4,
        aspect: f32,
        tex_w: f32,
        tex_h: f32,
        p: &WarpParams,
        lift: f32,
        tint: f32,
        hi_on: f32,
        tex_v_shift: f32,
    ) {
        let mut data = CameraCB {
            view_proj: [0.0; 16],
            aspect,
            plane_scale: p.plane_scale,
            tex_w,
            tex_h,
            lift_y: lift,
            cam_u: 0.5,
            cam_v: 0.5,
            tint,
            hi_on,
            hi_tex_w: tex_w * HI_SUPER,
            hi_tex_h: tex_h * HI_SUPER,
            hi_grid: p.hi_grid,
            tex_v_shift,
            _pad: [0.0; 3],
            tile_affine: *p.tile_affine,
            tile_cover: [p.tile_cover[0], p.tile_cover[1], p.tile_cover[2], p.tile_cover[3]],
            tile_cover_live: p.tile_cover[4],
            _pad2: [0.0; 3],
        };
        data.view_proj.copy_from_slice(view_proj.as_ref());
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if context.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).is_ok() {
                std::ptr::copy_nonoverlapping(
                    &data as *const CameraCB as *const u8,
                    mapped.pData as *mut u8,
                    std::mem::size_of::<CameraCB>(),
                );
                context.Unmap(&self.cb, 0);
            }
        }
    }

    // warp the world fbo in place: copy it to staging, clear it, then draw
    // the ground plane, lifted planes and billboards back onto it
    #[allow(clippy::too_many_arguments)]
    pub fn warp_in_place(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        texture: &ID3D11Texture2D,
        layers: &WarpLayers,
        p: &WarpParams,
        billboards: &[BillboardUv],
    ) {
        let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut tex_desc) };
        if !self.ensure_staging(device, tex_desc.Width, tex_desc.Height, tex_desc.Format) {
            return;
        }
        let (tex_w, tex_h) = (tex_desc.Width as f32, tex_desc.Height as f32);
        let aspect = tex_w / tex_h;
        let view_proj =
            build_view_proj(p.yaw, p.pitch, aspect, p.zoom, p.plane_scale, p.fps_eye_h);

        // publish the transform for cursor picking (window px space)
        crate::picking::set_transform(&view_proj, aspect, self.win_w, self.win_h);

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

            let hi_any = p.tile_cover.iter().any(|&cv| cv > 0.01);

            // 1. ground plane (with hi-res ground tiles composited over it)
            let ground_hi_on = if layers.ground_hi.is_some() && hi_any { 1.0 } else { 0.0 };
            self.write_cb(context, &view_proj, aspect, tex_w, tex_h, p, 0.0, 1.0, ground_hi_on, 0.0);

            context.ClearRenderTargetView(rtv, &[0.0f32, 0.0, 0.0, 0.0]);
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
            context.VSSetShader(&self.plane_vs, None);
            context.PSSetShader(&self.plane_ps, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.PSSetShaderResources(
                0,
                Some(&[Some(self.staging_srv.clone()), layers.ground_hi.cloned()]),
            );
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(4, 0);

            let blend_factor = [0.0f32; 4];

            // 2. belt plane: a small stack of slices from the ground up to the
            // lift height, darker at the bottom — a cheap extrusion with edges
            if let Some(bsrv) = layers.belt {
                context.PSSetShaderResources(0, Some(&[Some(bsrv.clone())]));
                context.OMSetBlendState(&self.premult_blend, Some(&blend_factor), 0xffffffff);
                const BELT_EDGE_SLICES: u32 = 3;
                for i in 1..=BELT_EDGE_SLICES {
                    let f = i as f32 / BELT_EDGE_SLICES as f32;
                    let tint = 0.35 + 0.65 * f;
                    self.write_cb(
                        context, &view_proj, aspect, tex_w, tex_h, p,
                        p.belt_lift * f, tint, 0.0, 0.0,
                    );
                    context.Draw(4, 0);
                }
                context.OMSetBlendState(None, None, 0xffffffff);
            }

            // 3. elevated-rail deck: slices concentrated just under the top so
            // it reads as a thin platform, not a wall down to the ground
            if let Some(esrv) = layers.elevated {
                if p.elevated_lift > 0.0 {
                    context.PSSetShaderResources(0, Some(&[Some(esrv.clone())]));
                    context.OMSetBlendState(&self.premult_blend, Some(&blend_factor), 0xffffffff);
                    const DECK_SLICES: u32 = 3;
                    for i in 1..=DECK_SLICES {
                        let f = i as f32 / DECK_SLICES as f32;
                        let h = p.elevated_lift * (0.92 + 0.08 * f);
                        let tint = 0.4 + 0.6 * f;
                        self.write_cb(
                            context, &view_proj, aspect, tex_w, tex_h, p, h, tint, 0.0, 0.0,
                        );
                        context.Draw(4, 0);
                    }
                    context.OMSetBlendState(None, None, 0xffffffff);
                }
            }

            // 4. billboards (or the object layer flat, if nothing was recorded)
            if let Some(osrv) = layers.object {
                let object_hi_on = if layers.object_hi.is_some() && hi_any { 1.0 } else { 0.0 };
                self.write_cb(
                    context, &view_proj, aspect, tex_w, tex_h, p, 0.0, 1.0, object_hi_on, 0.0,
                );
                context.OMSetBlendState(&self.premult_blend, Some(&blend_factor), 0xffffffff);
                // t2 = the warped ground plane: billboards over unrevealed
                // (black) ground vanish with it
                context.PSSetShaderResources(
                    0,
                    Some(&[
                        Some(osrv.clone()),
                        layers.object_hi.cloned(),
                        Some(self.staging_srv.clone()),
                    ]),
                );
                context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                let n = billboards.len().min(MAX_BILLBOARDS);
                if n > 0 {
                    self.draw_billboards(context, &billboards[..n], aspect, p);
                } else {
                    // fallback: composite the object layer flat on the ground
                    self.write_cb(context, &view_proj, aspect, tex_w, tex_h, p, 0.0, 1.0, 0.0, 0.0);
                    context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                    context.IASetInputLayout(None);
                    context.VSSetShader(&self.plane_vs, None);
                    context.PSSetShader(&self.plane_ps, None);
                    context.Draw(4, 0);
                }
                context.OMSetBlendState(None, None, 0xffffffff);
            }

            // 5. wire plane, drawn after the billboards so wires stay visible.
            // content is shifted south by the lift height: wire pixels attach
            // at sprite-top rows, north of the poles, in the flat capture
            if let Some(wsrv) = layers.wire {
                if p.wire_lift > 0.0 {
                    self.write_cb(
                        context, &view_proj, aspect, tex_w, tex_h, p,
                        p.wire_lift, 1.0, 0.0, -p.wire_lift / (2.0 * p.plane_scale),
                    );
                    context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                    context.IASetInputLayout(None);
                    context.VSSetShader(&self.plane_vs, None);
                    context.PSSetShader(&self.plane_ps, None);
                    context.PSSetShaderResources(0, Some(&[Some(wsrv.clone()), None]));
                    context.OMSetBlendState(&self.premult_blend, Some(&blend_factor), 0xffffffff);
                    context.Draw(4, 0);
                    context.OMSetBlendState(None, None, 0xffffffff);
                }
            }

            // restore state
            let null_srvs: [Option<ID3D11ShaderResourceView>; 3] = [None, None, None];
            context.PSSetShaderResources(0, Some(&null_srvs));
            context.OMSetRenderTargets(Some(&prev_rtvs), prev_dsv.as_ref());
            if num_vp > 0 {
                context.RSSetViewports(Some(&prev_vps[..num_vp as usize]));
            }
            context.RSSetState(prev_rs.as_ref());
        }
    }

    // build and draw the billboard quads (positions in warp-plane space)
    unsafe fn draw_billboards(
        &self,
        context: &ID3D11DeviceContext,
        billboards: &[BillboardUv],
        aspect: f32,
        p: &WarpParams,
    ) {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if context
                .Map(&self.entity_vb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .is_err()
            {
                return;
            }
            let dst = mapped.pData as *mut f32;
            let mut o = 0usize;
            let mut push = |x: f32, y: f32, z: f32, u: f32, v: f32, s: f32, fu: f32, fv: f32| {
                *dst.add(o) = x;
                *dst.add(o + 1) = y;
                *dst.add(o + 2) = z;
                *dst.add(o + 3) = u;
                *dst.add(o + 4) = v;
                *dst.add(o + 5) = s;
                *dst.add(o + 6) = fu;
                *dst.add(o + 7) = fv;
                o += 8;
            };
            let ps = p.plane_scale;
            // quad width runs along the camera's right vector (cos yaw, 0,
            // sin yaw) — cylindrical billboards, never edge-on
            let (sy, cyaw) = p.yaw.to_radians().sin_cos();
            let (wdx, wdz) = (cyaw, sy);
            for bb in billboards {
                let uc = bb.pu;
                // ground anchor = the entity's feet (guard against garbage)
                let foot = if (bb.v_foot - bb.v_base).abs() < 0.95 { bb.v_foot } else { bb.v_base };
                let s = bb.sel;
                if bb.flat {
                    // flat quad exactly where its pixels are in the capture,
                    // slightly above the ground so it wins the paint order
                    let fx0 = (bb.u0 - 0.5) * 2.0 * aspect * ps;
                    let fx1 = (bb.u1 - 0.5) * 2.0 * aspect * ps;
                    let fz_t = (0.5 - bb.v_top) * 2.0 * ps;
                    let fz_b = (0.5 - bb.v_base) * 2.0 * ps;
                    let y = 0.002 * ps;
                    push(fx0, y, fz_b, bb.u0, bb.v_base, s, uc, foot);
                    push(fx1, y, fz_b, bb.u1, bb.v_base, s, uc, foot);
                    push(fx0, y, fz_t, bb.u0, bb.v_top, s, uc, foot);
                    push(fx0, y, fz_t, bb.u0, bb.v_top, s, uc, foot);
                    push(fx1, y, fz_b, bb.u1, bb.v_base, s, uc, foot);
                    push(fx1, y, fz_t, bb.u1, bb.v_top, s, uc, foot);
                    continue;
                }
                let gx = (uc - 0.5) * 2.0 * aspect * ps;
                let gz = (0.5 - foot) * 2.0 * ps;
                let hw = 0.5 * (bb.u1 - bb.u0) * 2.0 * aspect * ps;
                // heights of the sprite's bottom/top edges above the feet,
                // plus fly_lift so flying robots hover off the ground
                let y_base = (foot - bb.v_base) * 2.0 * ps * STAND_SCALE + bb.fly_lift;
                let y_top = (foot - bb.v_top) * 2.0 * ps * STAND_SCALE + bb.fly_lift;
                let (lx, lz) = (gx - wdx * hw, gz - wdz * hw);
                let (rx, rz) = (gx + wdx * hw, gz + wdz * hw);
                push(lx, y_base, lz, bb.u0, bb.v_base, s, uc, foot);
                push(rx, y_base, rz, bb.u1, bb.v_base, s, uc, foot);
                push(lx, y_top, lz, bb.u0, bb.v_top, s, uc, foot);
                push(lx, y_top, lz, bb.u0, bb.v_top, s, uc, foot);
                push(rx, y_base, rz, bb.u1, bb.v_base, s, uc, foot);
                push(rx, y_top, rz, bb.u1, bb.v_top, s, uc, foot);
            }
            context.Unmap(&self.entity_vb, 0);

            context.IASetInputLayout(&self.entity_layout);
            context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(self.entity_vb.clone())),
                Some(&32u32),
                Some(&0u32),
            );
            context.VSSetShader(&self.entity_vs, None);
            context.PSSetShader(&self.entity_ps, None);
            context.Draw((billboards.len() * 6) as u32, 0);
            context.IASetInputLayout(None);
            let null_vb: [Option<ID3D11Buffer>; 1] = [None];
            context.IASetVertexBuffers(0, 1, Some(null_vb.as_ptr()), Some(&0u32), Some(&0u32));
        }
    }

    // composite a captured layer back flat over the fbo — used when layers
    // were diverted but the warp ended up not running this frame (camera
    // returned to vanilla mid-frame); otherwise they'd vanish for a frame
    pub fn composite_flat(
        &self,
        context: &ID3D11DeviceContext,
        srv: &ID3D11ShaderResourceView,
        fbo_w: u32,
        fbo_h: u32,
    ) {
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: fbo_w as f32,
                Height: fbo_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            context.RSSetViewports(Some(&[viewport]));
            // zero the cb: texW = 0 selects the plain passthrough sample
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if context.Map(&self.cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).is_ok() {
                std::ptr::write_bytes(mapped.pData as *mut u8, 0, std::mem::size_of::<CameraCB>());
                context.Unmap(&self.cb, 0);
            }
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.IASetInputLayout(None);
            context.VSSetShader(&self.flat_vs, None);
            context.PSSetShader(&self.plane_ps, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            let blend_factor = [0.0f32; 4];
            context.OMSetBlendState(&self.premult_blend, Some(&blend_factor), 0xffffffff);
            context.Draw(4, 0);
            context.OMSetBlendState(None, None, 0xffffffff);
            let null_srvs: [Option<ID3D11ShaderResourceView>; 1] = [None];
            context.PSSetShaderResources(0, Some(&null_srvs));
        }
    }
}

// camera that tilts (pitch) and orbits (yaw) around the player at the plane
// origin. first person places the eye at the player instead
fn build_view_proj(
    yaw: f32,
    pitch: f32,
    aspect: f32,
    zoom: f32,
    plane_scale: f32,
    fps_eye_h: f32,
) -> Mat4 {
    if crate::camera::fps_mode() {
        // eye at the player at head height; yaw 0 faces north, pitch ~25 =
        // horizon. forward matches the orbit camera's viewing direction
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
    let az = yaw.to_radians();
    let el = pitch.to_radians(); // 90 = straight above (top-down)

    let fov_rad = 60.0f32.to_radians();
    let base_dist = 1.0 / (fov_rad / 2.0).tan();
    let mut dist = base_dist * zoom;

    // keep the camera above the finite plane (its horizontal reach is
    // cos(el)*dist; the plane edge is at plane_scale)
    let (se, ce) = el.sin_cos();
    if ce > 0.01 {
        dist = dist.min(plane_scale * 0.95 / ce);
    }

    // spherical orbit around the origin; up = the elevation tangent, which
    // keeps yaw working even at exact top-down
    let (sa, ca) = az.sin_cos();
    let dir = Vec3::new(ce * sa, se, -ce * ca);
    let eye = dir * dist;
    let up = Vec3::new(-se * sa, ce, se * ca);
    let view = Mat4::look_at_lh(eye, Vec3::ZERO, up);
    let proj = Mat4::perspective_lh(fov_rad, aspect, 0.01, 100.0);

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

fn create_vs(device: &ID3D11Device, source: &str) -> Result<ID3D11VertexShader> {
    let bytecode = compile_shader(source, "vs_5_0")?;
    unsafe {
        let mut out = None;
        device.CreateVertexShader(&bytecode, None, Some(&mut out))?;
        out.context("CreateVertexShader")
    }
}

fn create_ps(device: &ID3D11Device, source: &str) -> Result<ID3D11PixelShader> {
    let bytecode = compile_shader(source, "ps_5_0")?;
    unsafe {
        let mut out = None;
        device.CreatePixelShader(&bytecode, None, Some(&mut out))?;
        out.context("CreatePixelShader")
    }
}

fn compile_shader(source: &str, target: &str) -> Result<Vec<u8>> {
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

// standalone D3D11 viewer for one GLB. loads base-color textures the same way
// the mod's gltf_model.rs does (per-primitive material -> decoded rgba ->
// R8G8B8A8 SRV + linear-wrap sampler, like model_renderer.rs) and spins it.
// static preview only: node transforms are baked, no skinning/animation.

use anyhow::{Context, Result};
use glam::{Mat3, Mat4, Vec3};
use std::ffi::c_void;
use windows::core::{s, w, PCSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const W: u32 = 1280;
const H: u32 = 720;

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
}

#[repr(C)]
struct ModelCB {
    mvp: [f32; 16],
    model: [f32; 16],
    light: [f32; 4],
    base_color: [f32; 4],
    flags: [f32; 4], // x = has base-color texture
}

const HLSL: &str = r#"
cbuffer CB : register(b0) {
    float4x4 mvp;
    float4x4 model;
    float4   light;
    float4   base_color;
    float4   flags;
};
Texture2D    tex0 : register(t0);
SamplerState samp : register(s0);
struct VSIn  { float3 pos : POSITION; float3 nrm : NORMAL; float2 uv : TEXCOORD; };
struct VSOut { float4 pos : SV_POSITION; float3 nrm : NORMAL; float2 uv : TEXCOORD; };

VSOut vs_main(VSIn i) {
    VSOut o;
    o.pos = mul(mvp, float4(i.pos, 1.0));
    o.nrm = mul((float3x3)model, i.nrm);
    o.uv  = i.uv;
    return o;
}
float4 ps_main(VSOut i) : SV_TARGET {
    float4 albedo = base_color;
    if (flags.x > 0.5) albedo *= tex0.Sample(samp, i.uv);
    float3 n = normalize(i.nrm);
    float d = saturate(dot(n, normalize(-light.xyz)));
    return float4(albedo.rgb * (0.25 + 0.75 * d), 1.0);
}
"#;

struct TexData {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

// one primitive: baked geometry + its material's base color/texture
struct PrimCpu {
    verts: Vec<Vertex>,
    indices: Vec<u32>,
    base_color: [f32; 4],
    tex: Option<TexData>,
}

fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        "M:/SteamLibrary/steamapps/common/Factorio/3d_mod_models/assembler.glb".into()
    });
    println!("loading {path}");
    let (prims, center, radius) = load_glb(&path)?;
    let (nv, ni, nt) = prims.iter().fold((0, 0, 0), |(v, i, t), p| {
        (v + p.verts.len(), i + p.indices.len(), t + p.tex.is_some() as usize)
    });
    println!("{} prims, {nv} vertices, {ni} indices, {nt} textured, radius {radius:.2}", prims.len());
    unsafe { run(&prims, center, radius) }
}

// ---- load the GLB into per-primitive baked geometry + textures -----------------

fn load_glb(path: &str) -> Result<(Vec<PrimCpu>, Vec3, f32)> {
    let (doc, buffers, images) = gltf::import(path).context("gltf import")?;
    let mut prims: Vec<PrimCpu> = Vec::new();
    let scene = doc.default_scene().or_else(|| doc.scenes().next()).context("no scene")?;
    for node in scene.nodes() {
        walk(&node, Mat4::IDENTITY, &buffers, &images, &mut prims);
    }

    let mut lo = Vec3::splat(f32::MAX);
    let mut hi = Vec3::splat(f32::MIN);
    for p in &prims {
        for v in &p.verts {
            let pt = Vec3::from(v.pos);
            lo = lo.min(pt);
            hi = hi.max(pt);
        }
    }
    let center = (lo + hi) * 0.5;
    let radius = ((hi - lo).length() * 0.5).max(0.001);
    Ok((prims, center, radius))
}

fn walk(
    node: &gltf::Node,
    parent: Mat4,
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
    prims: &mut Vec<PrimCpu>,
) {
    let world = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
    if let Some(mesh) = node.mesh() {
        let mut nrm_mat = Mat3::from_mat4(world).inverse().transpose();
        if !nrm_mat.is_finite() {
            nrm_mat = Mat3::from_mat4(world);
        }
        for prim in mesh.primitives() {
            let reader = prim.reader(|b| Some(&buffers[b.index()]));
            let positions: Vec<[f32; 3]> = match reader.read_positions() {
                Some(p) => p.collect(),
                None => continue,
            };
            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|n| n.collect())
                .unwrap_or_else(|| vec![[0.0, 1.0, 0.0]; positions.len()]);
            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|t| t.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);
            let mut verts = Vec::with_capacity(positions.len());
            for i in 0..positions.len() {
                let p = world.transform_point3(Vec3::from(positions[i]));
                let n = (nrm_mat * Vec3::from(normals[i])).normalize_or_zero();
                verts.push(Vertex { pos: p.into(), normal: n.into(), uv: uvs[i] });
            }
            let indices: Vec<u32> = reader
                .read_indices()
                .map(|i| i.into_u32().collect())
                .unwrap_or_else(|| (0..positions.len() as u32).collect());

            let mat = prim.material();
            let pbr = mat.pbr_metallic_roughness();
            let tex = pbr
                .base_color_texture()
                .and_then(|info| images.get(info.texture().source().index()))
                .and_then(to_rgba);
            prims.push(PrimCpu { verts, indices, base_color: pbr.base_color_factor(), tex });
        }
    }
    for child in node.children() {
        walk(&child, world, buffers, images, prims);
    }
}

// decode a gltf image to rgba8 (same formats gltf_model.rs::to_rgba handles)
fn to_rgba(img: &gltf::image::Data) -> Option<TexData> {
    use gltf::image::Format;
    let rgba = match img.format {
        Format::R8G8B8A8 => img.pixels.clone(),
        Format::R8G8B8 => img.pixels.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        Format::R8 => img.pixels.iter().flat_map(|&c| [c, c, c, 255]).collect(),
        other => {
            println!("unsupported texture format {other:?} — using base color factor");
            return None;
        }
    };
    Some(TexData { width: img.width, height: img.height, rgba })
}

// ---- D3D11 setup + render loop --------------------------------------------------

struct GpuPrim {
    vb: ID3D11Buffer,
    ib: ID3D11Buffer,
    index_count: u32,
    base_color: [f32; 4],
    srv: Option<ID3D11ShaderResourceView>,
}

unsafe fn run(prims: &[PrimCpu], center: Vec3, radius: f32) -> Result<()> {
    let hinstance = GetModuleHandleW(None)?;
    let hinst: HINSTANCE = hinstance.into();
    let class = w!("glb_viewer_window");

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        ..Default::default()
    };
    RegisterClassExW(&wc);

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!("GLB viewer (close to quit)"),
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        W as i32,
        H as i32,
        None,
        None,
        hinst,
        None,
    )?;

    let scd = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: W,
            Height: H,
            RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ..Default::default()
        },
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: TRUE,
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        ..Default::default()
    };

    let mut swapchain: Option<IDXGISwapChain> = None;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDeviceAndSwapChain(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_FLAG(0),
        None,
        D3D11_SDK_VERSION,
        Some(&scd),
        Some(&mut swapchain),
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .context("D3D11CreateDeviceAndSwapChain")?;
    let swapchain = swapchain.unwrap();
    let device = device.unwrap();
    let context = context.unwrap();

    let backbuf: ID3D11Texture2D = swapchain.GetBuffer(0)?;
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    device.CreateRenderTargetView(&backbuf, None, Some(&mut rtv))?;
    let rtv = rtv.unwrap();

    let depth_desc = D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_D24_UNORM_S8_UINT,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
        ..Default::default()
    };
    let mut depth_tex: Option<ID3D11Texture2D> = None;
    device.CreateTexture2D(&depth_desc, None, Some(&mut depth_tex))?;
    let depth_tex = depth_tex.unwrap();
    let mut dsv: Option<ID3D11DepthStencilView> = None;
    device.CreateDepthStencilView(&depth_tex, None, Some(&mut dsv))?;
    let dsv = dsv.unwrap();

    let vs_blob = compile(HLSL, s!("vs_main"), s!("vs_5_0"))?;
    let ps_blob = compile(HLSL, s!("ps_main"), s!("ps_5_0"))?;
    let vs_bytes = blob_bytes(&vs_blob);
    let ps_bytes = blob_bytes(&ps_blob);

    let mut vs: Option<ID3D11VertexShader> = None;
    device.CreateVertexShader(vs_bytes, None, Some(&mut vs))?;
    let vs = vs.unwrap();
    let mut ps: Option<ID3D11PixelShader> = None;
    device.CreatePixelShader(ps_bytes, None, Some(&mut ps))?;
    let ps = ps.unwrap();

    let elems = [
        input_elem(s!("POSITION"), DXGI_FORMAT_R32G32B32_FLOAT, 0),
        input_elem(s!("NORMAL"), DXGI_FORMAT_R32G32B32_FLOAT, 12),
        input_elem(s!("TEXCOORD"), DXGI_FORMAT_R32G32_FLOAT, 24),
    ];
    let mut layout: Option<ID3D11InputLayout> = None;
    device.CreateInputLayout(&elems, vs_bytes, Some(&mut layout))?;
    let layout = layout.unwrap();

    // one gpu buffer set + optional texture per primitive
    let mut gpu: Vec<GpuPrim> = Vec::with_capacity(prims.len());
    for p in prims {
        let vb = make_buffer(
            &device,
            p.verts.as_ptr() as *const c_void,
            std::mem::size_of_val(&p.verts[..]),
            D3D11_BIND_VERTEX_BUFFER,
        )?;
        let ib = make_buffer(
            &device,
            p.indices.as_ptr() as *const c_void,
            std::mem::size_of_val(&p.indices[..]),
            D3D11_BIND_INDEX_BUFFER,
        )?;
        let srv = p.tex.as_ref().map(|t| make_texture_srv(&device, t)).transpose()?;
        gpu.push(GpuPrim { vb, ib, index_count: p.indices.len() as u32, base_color: p.base_color, srv });
    }

    let sampler_desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_WRAP,
        AddressV: D3D11_TEXTURE_ADDRESS_WRAP,
        AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
        ..Default::default()
    };
    let mut sampler: Option<ID3D11SamplerState> = None;
    device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;
    let sampler = sampler.unwrap();

    let cb_desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<ModelCB>() as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    let mut cb: Option<ID3D11Buffer> = None;
    device.CreateBuffer(&cb_desc, None, Some(&mut cb))?;
    let cb = cb.unwrap();

    let rs_desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        DepthClipEnable: TRUE,
        ..Default::default()
    };
    let mut rs: Option<ID3D11RasterizerState> = None;
    device.CreateRasterizerState(&rs_desc, Some(&mut rs))?;
    context.RSSetState(&rs.unwrap());

    let viewport = D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: W as f32,
        Height: H as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    };

    let view = Mat4::look_at_rh(Vec3::new(0.0, 0.9, 2.6), Vec3::ZERO, Vec3::Y);
    let proj = Mat4::perspective_rh(60f32.to_radians(), W as f32 / H as f32, 0.05, 100.0);
    let fit = Mat4::from_scale(Vec3::splat(1.0 / radius)) * Mat4::from_translation(-center);

    let stride = std::mem::size_of::<Vertex>() as u32;
    let offset = 0u32;
    let start = std::time::Instant::now();

    let mut msg = MSG::default();
    'run: loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            if msg.message == WM_QUIT {
                break 'run;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let t = start.elapsed().as_secs_f32();
        let model = Mat4::from_rotation_y(t * 0.7) * fit;
        let mvp = proj * view * model;

        context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), &dsv);
        context.RSSetViewports(Some(&[viewport]));
        context.ClearRenderTargetView(&rtv, &[0.08, 0.09, 0.11, 1.0]);
        context.ClearDepthStencilView(&dsv, D3D11_CLEAR_DEPTH.0 as u32, 1.0, 0);
        context.IASetInputLayout(&layout);
        context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        context.VSSetShader(&vs, None);
        context.PSSetShader(&ps, None);
        context.PSSetSamplers(0, Some(&[Some(sampler.clone())]));

        for p in &gpu {
            let cbdata = ModelCB {
                mvp: mvp.to_cols_array(),
                model: model.to_cols_array(),
                light: [-0.4, -1.0, -0.6, 0.0],
                base_color: p.base_color,
                flags: [p.srv.is_some() as u32 as f32, 0.0, 0.0, 0.0],
            };
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::copy_nonoverlapping(
                &cbdata as *const ModelCB as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of::<ModelCB>(),
            );
            context.Unmap(&cb, 0);

            context.IASetVertexBuffers(0, 1, Some(&Some(p.vb.clone())), Some(&stride), Some(&offset));
            context.IASetIndexBuffer(&p.ib, DXGI_FORMAT_R32_UINT, 0);
            context.VSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            context.PSSetShaderResources(0, Some(&[p.srv.clone()]));
            context.DrawIndexed(p.index_count, 0, 0);
        }
        let _ = swapchain.Present(1, DXGI_PRESENT(0));
    }
    Ok(())
}

fn input_elem(name: PCSTR, format: DXGI_FORMAT, offset: u32) -> D3D11_INPUT_ELEMENT_DESC {
    D3D11_INPUT_ELEMENT_DESC {
        SemanticName: name,
        SemanticIndex: 0,
        Format: format,
        InputSlot: 0,
        AlignedByteOffset: offset,
        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
        InstanceDataStepRate: 0,
    }
}

unsafe fn compile(src: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let hr = D3DCompile(
        src.as_ptr() as *const c_void,
        src.len(),
        PCSTR::null(),
        None,
        None,
        entry,
        target,
        0,
        0,
        &mut code,
        Some(&mut errors),
    );
    if hr.is_err() {
        if let Some(e) = &errors {
            let msg = std::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize());
            anyhow::bail!("shader compile: {}", String::from_utf8_lossy(msg));
        }
        anyhow::bail!("shader compile failed: {hr:?}");
    }
    code.context("no shader bytecode")
}

unsafe fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
}

unsafe fn make_buffer(
    device: &ID3D11Device,
    data: *const c_void,
    size: usize,
    bind: D3D11_BIND_FLAG,
) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: size as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: bind.0 as u32,
        ..Default::default()
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: data, ..Default::default() };
    let mut buf: Option<ID3D11Buffer> = None;
    device.CreateBuffer(&desc, Some(&init), Some(&mut buf))?;
    buf.context("CreateBuffer returned null")
}

// immutable rgba texture + SRV (mirrors model_renderer.rs::make_texture_srv)
unsafe fn make_texture_srv(device: &ID3D11Device, t: &TexData) -> Result<ID3D11ShaderResourceView> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: t.width,
        Height: t.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: t.rgba.as_ptr() as *const _,
        SysMemPitch: t.width * 4,
        ..Default::default()
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    device.CreateTexture2D(&desc, Some(&data), Some(&mut tex))?;
    let tex = tex.context("CreateTexture2D")?;
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
    srv.context("CreateShaderResourceView")
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

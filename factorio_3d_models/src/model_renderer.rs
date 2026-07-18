// d3d11 pipeline that draws glb models at every replaced entity, directly
// onto the world fbo after (or instead of) the warp. one shared pipeline,
// per-model gpu buffers created lazily as models finish loading. uses its
// own depth buffer so model triangles sort against each other; the ground
// never writes depth, so models always sit on top of it.

use crate::entities::Instance;
use crate::gltf_model::ModelData;
use crate::settings;
use crate::tuning;
use anyhow::{Context, Result};
use glam::{Mat4, Quat, Vec3, Vec4};
use std::collections::HashMap;
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

// bone matrices per skinned draw (c4d rigs are small; warn when exceeded)
pub const MAX_BONES: usize = 96;

// lamp point lights per frame
pub const MAX_LAMPS: usize = 16;

// bytes of one 4x4 f32 matrix (gpu buffer sizing)
const MAT4_BYTES: usize = 64;

#[repr(C)]
#[derive(Clone, Copy)]
struct ModelCB {
    mvp: [f32; 16],
    model: [f32; 16],
    base_color: [f32; 4],
    // x = has texture, y = morph weight, z = pass (0 lit, 1 shadow, 2 wire),
    // w = skinned
    opts: [f32; 4],
    // xyz = direction the sunlight travels
    light: [f32; 4],
    // xy = uv scroll on upward faces (belt treads), z = global alpha,
    // w = 1 when this prim flows along the track path (tank chain)
    misc: [f32; 4],
    // xy = plane-space clip direction, zw = clip origin (composed junctions)
    clipv: [f32; 4],
    // per-prim material: x metallic, y roughness, z emissive strength, w night
    mat: [f32; 4],
    // xyz authored emissive color, w = 1 when an emissive texture is bound (t1)
    emissive: [f32; 4],
    // xyz world-space camera position (for specular), w exposure
    cam: [f32; 4],
    // xyz sky ambient color, w ambient strength
    sky: [f32; 4],
    // xyz ground-bounce ambient color, w rim-light strength
    ground: [f32; 4],
    // xyz sun light color, w sun strength
    sun: [f32; 4],
    // x night ambient floor, y specular scale, z env-reflection scale,
    // w flicker amount
    params: [f32; 4],
    // pbr texture flags: x normal map bound (t2), y metallic-roughness map
    // bound (t3), z mr map is ORM-packed (r channel = baked ao),
    // w seconds running (flicker phase)
    pbr: [f32; 4],
    // x = world-space height above which pixels clip (0 = off): the local
    // player's head in first person
    clipy: [f32; 4],
    // lamp point lights: xyz plane-space position, w falloff radius
    lamps: [[f32; 4]; MAX_LAMPS],
    // x = active lamp count, y = lamp strength
    lamp_meta: [f32; 4],
}

// chain loop path for the conveyor VS (tank tracks)
pub const MAX_TRACK_PTS: usize = 64;

#[repr(C)]
#[derive(Clone, Copy)]
struct TrackCB {
    pts: [[f32; 4]; MAX_TRACK_PTS], // xyz point (node-local), w cum arc length
    meta: [f32; 4],                 // x count, y total length, z/w scroll offset per side
    lat: [f32; 4],                  // xyz loop plane normal
}

// hlsl lives in src/shaders/; common declarations are prepended to each stage
const MODEL_HLSL: &str = include_str!("shaders/model_common.hlsl");
const MODEL_VS: &str = include_str!("shaders/model_vs.hlsl");
const MODEL_PS: &str = include_str!("shaders/model_ps.hlsl");

struct GpuPrim {
    node: usize,
    skin: Option<usize>,
    vb: ID3D11Buffer,
    ib: ID3D11Buffer,
    index_count: u32,
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
    emissive: [f32; 3],
    srv: Option<ID3D11ShaderResourceView>,
    emissive_srv: Option<ID3D11ShaderResourceView>,
    normal_srv: Option<ID3D11ShaderResourceView>,
    mr_srv: Option<ID3D11ShaderResourceView>,
    mr_has_ao: bool,
    tintable: bool,
    // gltf material name, for per-material draw rules (accumulator arcs)
    mat_name: String,
}

// gpu-side copy of one model, plus its cpu data for animation sampling
struct GpuModel {
    data: Arc<ModelData>,
    // prebuilt chain-path cbuffer contents (offset patched in per instance)
    track: Option<Box<TrackCB>>,
    prims: Vec<GpuPrim>,
    // rest-pose xz extent (widest axis) — footprint fitting reference.
    // models sit on their own origin: no recentering
    extent_xz: f32,
    // rest-pose matrices, shared by every idle instance (the common case) —
    // recomputing them per instance per frame was a hitch source
    rest_worlds: Arc<Vec<Mat4>>,
    rest_skins: Arc<Vec<Vec<Mat4>>>,
}

// wire mesh budget: dynamic VB, rebuilt per frame from the captured spans
const WIRE_SEGS: usize = 8;
const WIRE_MAX_VERTS: usize = 128 * 1024;

// tiles <-> plane-space mapping for this frame's view rect
struct ViewMap {
    left: f32, // view rect in tiles
    top: f32,
    span_x: f32,
    span_y: f32,
    aspect: f32,
    plane_scale: f32,
    tile_to_plane: f32,
}

impl ViewMap {
    // entity center -> fbo uv (0..1 across the visible world)
    fn uv(&self, x: f32, y: f32) -> (f32, f32) {
        ((x - self.left) / self.span_x, (y - self.top) / self.span_y)
    }

    // fbo uv -> plane-space xz (the warp plane the models stand on)
    fn plane(&self, u: f32, v: f32) -> (f32, f32) {
        (
            (u - 0.5) * 2.0 * self.aspect * self.plane_scale,
            (0.5 - v) * 2.0 * self.plane_scale,
        )
    }

    fn on_screen(u: f32, v: f32) -> bool {
        (-0.2..=1.2).contains(&u) && (-0.2..=1.2).contains(&v)
    }
}

// one instance ready to draw: node matrices sampled once, reused by both passes
struct Prepared {
    world: Mat4, // instance transform, node matrices NOT included
    morph: f32,
    key: &'static str,
    uv_scroll: [f32; 2],
    clip: [f32; 4],
    tint: [f32; 4], // player color onto tintable prims, a=0 = none
    active: bool,   // live working state (gates active-only prims)
    alpha: f32,     // per-instance opacity (ghost-entity previews < 1)
    track_offset: Option<(f32, f32)>, // chain scroll per side (conveyor VS)
    head_clip: f32, // clip pixels above this world height (0 = off)
    node_worlds: Arc<Vec<Mat4>>,
    skin_mats: Arc<Vec<Vec<Mat4>>>, // per skin index
    // instancing-eligible: static rest pose, no per-instance shader state.
    // precomputed here (the model is at hand) so build_plan does no lookups
    plain: bool,
}

// frame-constant lighting + pass settings, cloned into every prim's cbuffer
struct FrameCB {
    base: ModelCB,
    alpha: f32, // < 1 while ALT is held (see-through for the alt overlay)
    night: f32,
    rough_mul: f32,
    emissive_mul: f32,
}

pub struct ModelRenderer {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    cb: ID3D11Buffer,
    bones_cb: ID3D11Buffer,
    track_cb: ID3D11Buffer,
    wire_vb: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    rs: ID3D11RasterizerState,
    dss: ID3D11DepthStencilState,
    // ground-shadow pass: alpha blend + stencil-once (no double darkening)
    shadow_dss: ID3D11DepthStencilState,
    shadow_blend: ID3D11BlendState,
    models: crate::util::FxHashMap<&'static str, GpuModel>,
    depth: Option<(u32, u32, ID3D11DepthStencilView)>,
    // per-instance world matrices for instanced draws (buffer, srv, capacity
    // in matrices) — grown on demand, refilled every frame
    inst_buf: Option<(ID3D11Buffer, ID3D11ShaderResourceView, usize)>,
}

// one instanced batch: many rest-pose copies of one model, drawn with a
// single DrawIndexedInstanced per prim (belt fields, items on belts).
// matrices are laid out per prim: slot = base + prim_index * members.len() + i
struct InstGroup {
    key: &'static str,
    uv_scroll: [f32; 2],
    active: bool,
    members: Vec<usize>, // indices into prepared
    base: usize,
}

impl ModelRenderer {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let vs_code = crate::warp::compile_shader(&format!("{MODEL_HLSL}{MODEL_VS}"), "vs_5_0")?;
        let vs = unsafe {
            let mut out = None;
            device.CreateVertexShader(&vs_code, None, Some(&mut out))?;
            out.context("CreateVertexShader model")?
        };
        let ps = crate::warp::create_ps(device, &format!("{MODEL_HLSL}{MODEL_PS}"))?;

        // interleaved vertex: see gltf_model::VERTEX_FLOATS
        use windows::core::s;
        let layout_desc = [
            vtx(s!("POSITION"), 0, DXGI_FORMAT_R32G32B32_FLOAT, 0),
            vtx(s!("NORMAL"), 0, DXGI_FORMAT_R32G32B32_FLOAT, 12),
            vtx(s!("TEXCOORD"), 0, DXGI_FORMAT_R32G32_FLOAT, 24),
            vtx(s!("POSITION"), 1, DXGI_FORMAT_R32G32B32_FLOAT, 32),
            vtx(s!("NORMAL"), 1, DXGI_FORMAT_R32G32B32_FLOAT, 44),
            vtx(s!("BLENDINDICES"), 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 56),
            vtx(s!("BLENDWEIGHT"), 0, DXGI_FORMAT_R32G32B32A32_FLOAT, 72),
        ];
        let layout = unsafe {
            let mut lo = None;
            device.CreateInputLayout(&layout_desc, &vs_code, Some(&mut lo))?;
            lo.context("CreateInputLayout model")?
        };

        let cb = dynamic_buffer(device, std::mem::size_of::<ModelCB>(), D3D11_BIND_CONSTANT_BUFFER)?;
        let bones_cb = dynamic_buffer(device, MAX_BONES * MAT4_BYTES, D3D11_BIND_CONSTANT_BUFFER)?;
        let track_cb = dynamic_buffer(device, std::mem::size_of::<TrackCB>(), D3D11_BIND_CONSTANT_BUFFER)?;
        let wire_vb = dynamic_buffer(
            device,
            WIRE_MAX_VERTS * crate::gltf_model::VERTEX_FLOATS * 4,
            D3D11_BIND_VERTEX_BUFFER,
        )?;

        // anisotropic + full mip range: the 3d camera sees belts at glancing
        // angles, and MaxLOD defaults to 0 which would disable the mip chain
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_ANISOTROPIC,
            MaxAnisotropy: 8,
            AddressU: D3D11_TEXTURE_ADDRESS_WRAP,
            AddressV: D3D11_TEXTURE_ADDRESS_WRAP,
            AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let sampler = unsafe {
            let mut s = None;
            device.CreateSamplerState(&sampler_desc, Some(&mut s))?;
            s.context("CreateSamplerState model")?
        };

        // materials are double-sided; never cull
        let rs_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let rs = unsafe {
            let mut r = None;
            device.CreateRasterizerState(&rs_desc, Some(&mut r))?;
            r.context("CreateRasterizerState model")?
        };

        let dss_desc = D3D11_DEPTH_STENCIL_DESC {
            DepthEnable: true.into(),
            DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL,
            DepthFunc: D3D11_COMPARISON_LESS,
            ..Default::default()
        };
        let dss = unsafe {
            let mut d = None;
            device.CreateDepthStencilState(&dss_desc, Some(&mut d))?;
            d.context("CreateDepthStencilState model")?
        };

        // shadow pass: no depth writes; stencil "first fragment only" —
        // flattened geometry overlaps itself and would double-darken otherwise
        let shadow_face = D3D11_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D11_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D11_STENCIL_OP_KEEP,
            StencilPassOp: D3D11_STENCIL_OP_INCR_SAT,
            StencilFunc: D3D11_COMPARISON_EQUAL,
        };
        let shadow_dss_desc = D3D11_DEPTH_STENCIL_DESC {
            DepthEnable: true.into(),
            DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO,
            DepthFunc: D3D11_COMPARISON_LESS_EQUAL,
            StencilEnable: true.into(),
            StencilReadMask: 0xFF,
            StencilWriteMask: 0xFF,
            FrontFace: shadow_face,
            BackFace: shadow_face,
        };
        let shadow_dss = unsafe {
            let mut d = None;
            device.CreateDepthStencilState(&shadow_dss_desc, Some(&mut d))?;
            d.context("CreateDepthStencilState shadow")?
        };

        let shadow_blend_desc = D3D11_BLEND_DESC {
            RenderTarget: {
                let mut rt = [D3D11_RENDER_TARGET_BLEND_DESC::default(); 8];
                rt[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                    BlendEnable: true.into(),
                    SrcBlend: D3D11_BLEND_SRC_ALPHA,
                    DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                    BlendOp: D3D11_BLEND_OP_ADD,
                    SrcBlendAlpha: D3D11_BLEND_ZERO,
                    DestBlendAlpha: D3D11_BLEND_ONE,
                    BlendOpAlpha: D3D11_BLEND_OP_ADD,
                    RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                };
                rt
            },
            ..Default::default()
        };
        let shadow_blend = unsafe {
            let mut b = None;
            device.CreateBlendState(&shadow_blend_desc, Some(&mut b))?;
            b.context("CreateBlendState shadow")?
        };

        Ok(Self {
            vs,
            ps,
            layout,
            cb,
            bones_cb,
            track_cb,
            wire_vb,
            sampler,
            rs,
            dss,
            shadow_dss,
            shadow_blend,
            models: Default::default(),
            depth: None,
            inst_buf: None,
        })
    }

    // upload the chain path with this instance's per-side offsets patched in
    unsafe fn upload_track(
        &self,
        context: &ID3D11DeviceContext,
        base: &TrackCB,
        offset: (f32, f32),
    ) {
        let mut data = *base;
        data.meta[2] = offset.0;
        data.meta[3] = offset.1;
        unsafe { upload(context, &self.track_cb, &data) };
    }

    // write one skin's matrices into the bones cbuffer
    unsafe fn upload_bones(&self, context: &ID3D11DeviceContext, mats: &[Mat4]) {
        let n = mats.len().min(MAX_BONES);
        unsafe { upload_raw(context, &self.bones_cb, mats.as_ptr() as *const u8, n * MAT4_BYTES) };
    }

    unsafe fn upload_cb(&self, context: &ID3D11DeviceContext, data: &ModelCB) {
        unsafe { upload(context, &self.cb, data) };
    }

    // upload a model's buffers on first use. `budget` caps uploads per frame
    // so a save full of new model types doesn't stall one frame with dozens
    // of gpu uploads — the rest appear over the next frames
    fn ensure_model(
        &mut self,
        device: &ID3D11Device,
        key: &'static str,
        budget: &mut u32,
    ) -> Option<&GpuModel> {
        if !self.models.contains_key(key) {
            if *budget == 0 {
                return None;
            }
            let data = crate::models::get(key)?;
            *budget -= 1;
            match upload_model(device, data) {
                Ok(gm) => {
                    self.models.insert(key, gm);
                }
                Err(e) => {
                    log::error!("[model] gpu upload failed for {key}: {e:#}");
                    return None;
                }
            }
        }
        self.models.get(key)
    }

    unsafe fn bind_prim(&self, context: &ID3D11DeviceContext, prim: &GpuPrim) {
        unsafe {
            context.IASetVertexBuffers(
                0,
                1,
                Some(&Some(prim.vb.clone())),
                Some(&((crate::gltf_model::VERTEX_FLOATS * 4) as u32)),
                Some(&0u32),
            );
            context.IASetIndexBuffer(&prim.ib, DXGI_FORMAT_R32_UINT, 0);
        }
    }

    // split the prepared instances into instanced groups (identical static
    // rest-pose copies of one model) and leftovers drawn one by one. this is
    // what keeps huge belt/item fields at playable draw-call counts
    fn build_plan(&self, prepared: &[Prepared]) -> (Vec<usize>, Vec<InstGroup>) {
        let mut map: crate::util::FxHashMap<(&'static str, [u32; 2], bool), Vec<usize>> =
            Default::default();
        let mut singles: Vec<usize> = Vec::new();
        for (i, p) in prepared.iter().enumerate() {
            if p.plain {
                map.entry((p.key, p.uv_scroll.map(f32::to_bits), p.active)).or_default().push(i);
            } else {
                singles.push(i);
            }
        }
        let mut groups: Vec<InstGroup> = Vec::new();
        for ((key, uv, active), members) in map {
            if members.len() < 2 {
                singles.extend(members);
            } else {
                groups.push(InstGroup {
                    key,
                    uv_scroll: uv.map(f32::from_bits),
                    active,
                    members,
                    base: 0,
                });
            }
        }
        (singles, groups)
    }

    // dynamic structured buffer holding this frame's instance matrices
    fn ensure_inst_buf(
        &mut self,
        device: &ID3D11Device,
        n: usize,
    ) -> Option<(ID3D11Buffer, ID3D11ShaderResourceView)> {
        let need = n.max(1);
        if !self.inst_buf.as_ref().is_some_and(|(_, _, cap)| *cap >= need) {
            let cap = need.next_power_of_two().max(1024);
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: (cap * MAT4_BYTES) as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
                StructureByteStride: MAT4_BYTES as u32,
            };
            let buf = unsafe {
                let mut b = None;
                device.CreateBuffer(&desc, None, Some(&mut b)).ok()?;
                b?
            };
            let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_UNKNOWN,
                ViewDimension: windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_BUFFER,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D11_BUFFER_SRV {
                        Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                        Anonymous2: D3D11_BUFFER_SRV_1 { NumElements: cap as u32 },
                    },
                },
            };
            let srv = unsafe {
                let mut v = None;
                device.CreateShaderResourceView(&buf, Some(&srv_desc), Some(&mut v)).ok()?;
                v?
            };
            self.inst_buf = Some((buf, srv, cap));
        }
        self.inst_buf.as_ref().map(|(b, v, _)| (b.clone(), v.clone()))
    }

    fn ensure_depth(&mut self, device: &ID3D11Device, w: u32, h: u32) -> Option<ID3D11DepthStencilView> {
        if let Some((dw, dh, dsv)) = &self.depth {
            if *dw == w && *dh == h {
                return Some(dsv.clone());
            }
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w.max(1),
            Height: h.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_D24_UNORM_S8_UINT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
            ..Default::default()
        };
        let tex = unsafe {
            let mut t = None;
            device.CreateTexture2D(&desc, None, Some(&mut t)).ok()?;
            t?
        };
        let dsv = unsafe {
            let mut d = None;
            device.CreateDepthStencilView(&tex, None, Some(&mut d)).ok()?;
            d?
        };
        self.depth = Some((w, h, dsv.clone()));
        Some(dsv)
    }

    // build the per-instance draw list; samples node matrices once per
    // instance (reused by both passes) and uploads new gpu models
    fn prepare_instances(
        &mut self,
        device: &ID3D11Device,
        instances: &[Instance],
        view: &ViewMap,
    ) -> Vec<Prepared> {
        let mut upload_budget = 2u32; // gpu uploads per frame
        // gltf is right-handed, the warp plane space is left-handed
        let flip = Mat4::from_scale(Vec3::new(1.0, 1.0, -1.0));
        let mut prepared: Vec<Prepared> = Vec::with_capacity(instances.len());
        for inst in instances {
            let (u, v) = view.uv(inst.x, inst.y);
            if !ViewMap::on_screen(u, v) {
                continue;
            }
            if self.ensure_model(device, inst.key, &mut upload_budget).is_none() {
                continue;
            }
            if inst.scale_ref != inst.key
                && self.ensure_model(device, inst.scale_ref, &mut upload_budget).is_none()
            {
                continue;
            }
            // the models aren't 1u = 1tile: fit the reference part's xz
            // extent to the entity footprint, same factor for every part
            let ref_extent = self.models[inst.scale_ref].extent_xz.max(0.01);
            let fit = inst.tiles / ref_extent * view.tile_to_plane * settings::MODEL_SCALE;
            let gm = &self.models[inst.key];
            let (px, pz) = view.plane(u, v);
            // a live override (gate opening progress) beats the glb's own
            // weight animation
            let morph = inst.morph.unwrap_or_else(|| {
                gm.data
                    .weight_anim
                    .as_ref()
                    .map(|a| a.sample(inst.anim_t))
                    .unwrap_or(0.0)
            });
            let yaw = Mat4::from_quat(Quat::from_rotation_y(
                inst.yaw + settings::MODEL_YAW_DEG.to_radians(),
            ));
            let roll = if inst.roll != 0.0 {
                Mat4::from_quat(Quat::from_rotation_z(inst.roll))
            } else {
                Mat4::IDENTITY
            };
            let mirror = if inst.mirror {
                Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0))
            } else {
                Mat4::IDENTITY
            };
            // no recentering: the model origin is trusted as the entity
            // center at ground level (silo shafts go below, clipped in PS)
            let py = inst.lift * view.tile_to_plane;
            let world = Mat4::from_translation(Vec3::new(px, py, pz))
                * Mat4::from_scale(Vec3::splat(fit))
                * yaw
                * roll
                * mirror
                * flip;
            // idle instances (the common case) share the cached rest pose
            let posed = gm.data.has_pose_nodes
                && (inst.turret_yaw != 0.0
                    || inst.track_phase != 0.0
                    || inst.track_phase_b != 0.0);
            let mut track_offset = None;
            let (node_worlds, skin_mats) = if inst.anim_t == 0.0 && !posed {
                (gm.rest_worlds.clone(), gm.rest_skins.clone())
            } else {
                // chain scroll: tiles driven -> model units, signed, per side
                let units_per_tile = ref_extent / inst.tiles.max(0.01);
                let speed = tuning::TANK_TRACK_SPEED.get();
                let adv = inst.track_phase * speed * units_per_tile;
                let adv_b = inst.track_phase_b * speed * units_per_tile;
                // with a loop path the conveyor VS moves the chain; without,
                // fall back to translating the node, wrapped at the tread pitch
                let node_scroll = match &gm.data.track_path {
                    Some(tp) => {
                        let wrap = tp.total.max(1e-3);
                        track_offset = Some((adv.rem_euclid(wrap), adv_b.rem_euclid(wrap)));
                        0.0
                    }
                    None => {
                        let period = tuning::TANK_TRACK_PERIOD.get().max(1e-3) * units_per_tile;
                        adv.rem_euclid(period)
                    }
                };
                let wheel_flip = if tuning::TANK_WHEEL_FLIP.get() > 0.5 { -1.0 } else { 1.0 };
                let pose = crate::gltf_model::Pose {
                    turret_yaw: inst.turret_yaw,
                    track_offset: node_scroll,
                    wheel_advance: adv * wheel_flip,
                };
                let worlds = gm.data.node_worlds_posed(inst.anim_t, &pose);
                let skins = (0..gm.data.skins.len())
                    .map(|s| gm.data.skin_matrices(s, &worlds))
                    .collect();
                (Arc::new(worlds), Arc::new(skins))
            };
            // composed junction half: clip plane through the entity center,
            // nudged slightly backward so the halves overlap in the middle
            let clip = if inst.clip_dir != [0.0; 2] {
                let eps = 0.05 * view.tile_to_plane;
                [
                    inst.clip_dir[0],
                    inst.clip_dir[1],
                    px - inst.clip_dir[0] * eps,
                    pz - inst.clip_dir[1] * eps,
                ]
            } else {
                [0.0; 4]
            };
            // first person: the fps eye sits at the plane origin, so the
            // player instance at the view center IS the local character —
            // clip its head (it would fill the camera). other players stand
            // away from the origin and keep theirs
            let head_clip = if crate::camera::fps_mode()
                && crate::models::is_player_key(inst.key)
                && px * px + pz * pz < (view.tile_to_plane * 1.0).powi(2)
            {
                let frac = tuning::PLAYER_HEAD_CLIP.get();
                if frac > 0.01 {
                    let (lo, hi) = (gm.data.aabb_min.y, gm.data.aabb_max.y);
                    py + (lo + (hi - lo) * frac.min(1.0)) * fit
                } else {
                    0.0
                }
            } else {
                0.0
            };
            let plain = track_offset.is_none()
                && head_clip == 0.0
                && clip == [0.0; 4]
                && inst.tint[3] <= 0.0
                && inst.transparency <= 0.001
                && morph == 0.0
                && Arc::ptr_eq(&node_worlds, &gm.rest_worlds);
            prepared.push(Prepared {
                world,
                morph,
                key: inst.key,
                uv_scroll: inst.uv_scroll,
                clip,
                tint: inst.tint,
                active: inst.active,
                alpha: 1.0 - inst.transparency.clamp(0.0, 1.0),
                track_offset,
                head_clip,
                node_worlds,
                skin_mats,
                plain,
            });
        }
        prepared
    }

    // one pass over every prepared instance's prims. shadow pass flattens the
    // geometry onto the ground (pre_world) and paints flat translucent black;
    // the lit pass binds the prim's material + textures
    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_prims(
        &self,
        context: &ID3D11DeviceContext,
        prepared: &[Prepared],
        singles: &[usize],
        groups: &[InstGroup],
        frame: &FrameCB,
        view_proj: &Mat4,
        shadow: Option<&Mat4>, // Some(flatten matrix) = shadow pass
    ) {
        // instanced batches: one DrawIndexedInstanced per (model, prim)
        let vp = match shadow {
            Some(flatten) => *view_proj * *flatten,
            None => *view_proj,
        };
        for g in groups {
            let gm = &self.models[g.key];
            let n = g.members.len();
            for (pi, prim) in gm.prims.iter().enumerate() {
                let arc = is_arc_prim(g.key, prim);
                if arc && !g.active {
                    continue;
                }
                let mut data = frame.base;
                data.mvp.copy_from_slice(vp.as_ref());
                data.model.copy_from_slice(Mat4::IDENTITY.as_ref());
                data.clipy = [0.0, 1.0, (g.base + pi * n) as f32, 0.0];
                let skinned = if prim.skin.is_some() { 1.0 } else { 0.0 };
                if shadow.is_some() {
                    data.base_color = [0.0, 0.0, 0.0, settings::SHADOW_ALPHA * frame.alpha];
                    data.opts = [0.0, 0.0, 1.0, skinned];
                    data.misc = [0.0, 0.0, 1.0, 0.0];
                } else {
                    data.base_color = prim.base_color;
                    data.opts =
                        [if prim.srv.is_some() { 1.0 } else { 0.0 }, 0.0, 0.0, skinned];
                    data.misc = [g.uv_scroll[0], g.uv_scroll[1], frame.alpha, 0.0];
                    fill_lit_material(&mut data, prim, frame, g.key, arc);
                }
                unsafe {
                    self.upload_cb(context, &data);
                    if let Some(sk) = prim.skin {
                        self.upload_bones(context, &prepared[g.members[0]].skin_mats[sk]);
                    }
                    self.bind_prim(context, prim);
                    if shadow.is_none() {
                        context.PSSetShaderResources(
                            0,
                            Some(&[
                                prim.srv.clone(),
                                prim.emissive_srv.clone(),
                                prim.normal_srv.clone(),
                                prim.mr_srv.clone(),
                            ]),
                        );
                    }
                    context.DrawIndexedInstanced(prim.index_count, n as u32, 0, 0, 0);
                }
            }
        }

        for &si in singles {
            let p = &prepared[si];
            // translucent ghosts cast no ground shadow
            if shadow.is_some() && p.alpha < 0.999 {
                continue;
            }
            let gm = &self.models[p.key];
            if let (Some(off), Some(tcb)) = (p.track_offset, gm.track.as_deref()) {
                unsafe { self.upload_track(context, tcb, off) };
            }
            for prim in &gm.prims {
                let arc = is_arc_prim(p.key, prim);
                if arc && !p.active {
                    continue;
                }
                let on_track = p.track_offset.is_some() && gm.data.nodes[prim.node].is_track;
                // skinned prims: bones replace the node transform
                let world = match prim.skin {
                    Some(_) => p.world,
                    None => p.world * p.node_worlds[prim.node],
                };
                let mvp = match shadow {
                    Some(flatten) => *view_proj * *flatten * world,
                    None => *view_proj * world,
                };
                let mut data = frame.base;
                data.mvp.copy_from_slice(mvp.as_ref());
                data.model.copy_from_slice(world.as_ref());
                data.clipv = p.clip;
                // both passes: wpos is unflattened, so the head's ground
                // shadow disappears together with the head
                data.clipy = [p.head_clip, 0.0, 0.0, 0.0];
                let skinned = if prim.skin.is_some() { 1.0 } else { 0.0 };
                if shadow.is_some() {
                    data.base_color = [0.0, 0.0, 0.0, settings::SHADOW_ALPHA * frame.alpha];
                    data.opts = [0.0, p.morph, 1.0, skinned];
                    data.misc = [0.0, 0.0, 1.0, if on_track { 1.0 } else { 0.0 }];
                } else {
                    data.base_color = prim.base_color;
                    // player color: multiplies the albedo (the shader also
                    // multiplies any texture on top)
                    if p.tint[3] > 0.0 && prim.tintable {
                        for i in 0..3 {
                            data.base_color[i] *= p.tint[i];
                        }
                    }
                    data.opts =
                        [if prim.srv.is_some() { 1.0 } else { 0.0 }, p.morph, 0.0, skinned];
                    data.misc = [
                        p.uv_scroll[0],
                        p.uv_scroll[1],
                        frame.alpha * p.alpha,
                        if on_track { 1.0 } else { 0.0 },
                    ];
                    fill_lit_material(&mut data, prim, frame, p.key, arc);
                }
                unsafe {
                    self.upload_cb(context, &data);
                    if let Some(s) = prim.skin {
                        self.upload_bones(context, &p.skin_mats[s]);
                    }
                    self.bind_prim(context, prim);
                    if shadow.is_none() {
                        context.PSSetShaderResources(
                            0,
                            Some(&[
                                prim.srv.clone(),
                                prim.emissive_srv.clone(),
                                prim.normal_srv.clone(),
                                prim.mr_srv.clone(),
                            ]),
                        );
                    }
                    context.DrawIndexed(prim.index_count, 0, 0);
                }
            }
        }
    }

    // pass 3: wires — one dynamic-VB draw per color group
    unsafe fn draw_wires(
        &self,
        context: &ID3D11DeviceContext,
        wire_groups: &HashMap<[u32; 4], Vec<f32>>,
        frame: &FrameCB,
        view_proj: &Mat4,
    ) {
        for (color_bits, verts) in wire_groups {
            let n_verts = verts.len() / crate::gltf_model::VERTEX_FLOATS;
            if n_verts == 0 {
                continue;
            }
            unsafe {
                upload_raw(context, &self.wire_vb, verts.as_ptr() as *const u8, verts.len() * 4);
                let mut data = frame.base;
                data.mvp.copy_from_slice(view_proj.as_ref());
                data.model.copy_from_slice(Mat4::IDENTITY.as_ref());
                data.base_color = color_bits.map(f32::from_bits);
                // opts.z = 2 -> the shader's flat bright-cable wire branch
                data.opts = [0.0, 0.0, 2.0, 0.0];
                data.misc = [0.0, 0.0, frame.alpha, 0.0];
                self.upload_cb(context, &data);
                context.IASetVertexBuffers(
                    0,
                    1,
                    Some(&Some(self.wire_vb.clone())),
                    Some(&((crate::gltf_model::VERTEX_FLOATS * 4) as u32)),
                    Some(&0u32),
                );
                context.PSSetShaderResources(0, Some(&[None, None, None, None]));
                context.Draw(n_verts as u32, 0);
            }
        }
    }

    // draw every visible instance. view_rect = (left, top, span_x, span_y)
    // in tiles; plane coordinates match the warp plane.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        fbo_w: u32,
        fbo_h: u32,
        view_proj: &Mat4,
        cam_eye: Vec3,
        plane_scale: f32,
        view_rect: (f32, f32, f32, f32),
        instances: &[Instance],
        wires: &[crate::entities::WireDraw],
    ) {
        if instances.is_empty() && wires.is_empty() {
            return;
        }
        let (left, top, span_x, span_y) = view_rect;
        if span_x < 0.5 || span_y < 0.5 {
            return;
        }
        let view = ViewMap {
            left,
            top,
            span_x,
            span_y,
            aspect: fbo_w as f32 / fbo_h.max(1) as f32,
            plane_scale,
            tile_to_plane: 2.0 * plane_scale / span_y,
        };

        // gpu model uploads happen in prepare, before the state save below
        let prepared = self.prepare_instances(device, instances, &view);
        let wire_groups = build_wire_verts(wires, &view);
        if prepared.is_empty() && wire_groups.is_empty() {
            return;
        }
        let Some(dsv) = self.ensure_depth(device, fbo_w, fbo_h) else { return };

        // instancing plan: repeated static models draw as one call per prim.
        // matrix layout matches draw_prims (per group, per prim, per member)
        let (singles, mut groups) = self.build_plan(&prepared);
        let mut mats: Vec<f32> = Vec::new();
        for g in &mut groups {
            g.base = mats.len() / 16;
            let gm = &self.models[g.key];
            for prim in &gm.prims {
                for &ii in &g.members {
                    let pr = &prepared[ii];
                    let w = match prim.skin {
                        Some(_) => pr.world,
                        None => pr.world * pr.node_worlds[prim.node],
                    };
                    mats.extend_from_slice(w.as_ref());
                }
            }
        }
        let inst = if mats.is_empty() {
            None
        } else {
            self.ensure_inst_buf(device, mats.len() / 16)
        };
        {
            static LOG_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if LOG_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 300 == 0 {
                log::info!(
                    "[model] draw plan: {} instances -> {} groups + {} singles ({} matrices)",
                    prepared.len(),
                    groups.len(),
                    singles.len(),
                    mats.len() / 16
                );
            }
        }

        let sun = Vec3::from(settings::SUN_DIR).normalize();
        let mut frame = frame_constants(cam_eye, sun);

        // on-screen lamps light their surroundings at night (point lights in
        // the model shader). positions in plane space, like the instances
        if frame.night > 0.01 {
            let mut n = 0;
            for inst in instances {
                if n == MAX_LAMPS {
                    break;
                }
                if !inst.key.contains("small-lamp") {
                    continue;
                }
                let (u, v) = view.uv(inst.x, inst.y);
                if !ViewMap::on_screen(u, v) {
                    continue;
                }
                let (px, pz) = view.plane(u, v);
                frame.base.lamps[n] = [
                    px,
                    0.5 * view.tile_to_plane, // light from bulb height
                    pz,
                    tuning::LAMP_RADIUS.get().max(0.1) * view.tile_to_plane,
                ];
                n += 1;
            }
            frame.base.lamp_meta = [n as f32, tuning::LAMP_STRENGTH.get(), 0.0, 0.0];
        }

        unsafe {
            // save EVERY piece of state this pass touches. factorio's own
            // renderer keeps bindings (index buffer!) alive across frames, so
            // anything left dirty here breaks its gui pass
            let saved = SavedState::save(context);
            if let Some((buf, srv)) = &inst {
                upload_raw(context, buf, mats.as_ptr() as *const u8, mats.len() * 4);
                context.VSSetShaderResources(4, Some(&[Some(srv.clone())]));
            }

            context.ClearDepthStencilView(
                &dsv,
                (D3D11_CLEAR_DEPTH.0 | D3D11_CLEAR_STENCIL.0) as u32,
                1.0,
                0,
            );
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), Some(&dsv));
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: fbo_w as f32,
                Height: fbo_h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            context.RSSetViewports(Some(&[viewport]));
            context.RSSetState(&self.rs);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(&self.layout);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(&self.ps, None);
            context.VSSetConstantBuffers(
                0,
                Some(&[
                    Some(self.cb.clone()),
                    Some(self.bones_cb.clone()),
                    Some(self.track_cb.clone()),
                ]),
            );
            context.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            let blend_factor = [0.0f32; 4];

            // pass 1: ground shadows — geometry flattened onto y=0 along the
            // sun direction, drawn once per pixel (stencil) with alpha blend.
            // skipped at far zoom (SHADOW_MAX_SPAN): sub-pixel shadows aren't
            // worth a second full geometry pass over tens of thousands of
            // instances
            let max_span = tuning::SHADOW_MAX_SPAN.get();
            let shadows_on = max_span <= 0.0 || span_y <= max_span;
            if sun.y < -0.01 && shadows_on {
                let flatten = Mat4::from_cols(
                    Vec4::new(1.0, 0.0, 0.0, 0.0),
                    Vec4::new(-sun.x / sun.y, 0.0, -sun.z / sun.y, 0.0),
                    Vec4::new(0.0, 0.0, 1.0, 0.0),
                    Vec4::new(0.0, 0.0, 0.0, 1.0),
                );
                context.OMSetDepthStencilState(Some(&self.shadow_dss), 0);
                context.OMSetBlendState(&self.shadow_blend, Some(&blend_factor), 0xffffffff);
                self.draw_prims(
                    context, &prepared, &singles, &groups, &frame, view_proj, Some(&flatten),
                );
            }

            // pass 2: the models themselves (lit by the same sun; alpha
            // blended so hold-ALT can fade them over the overlay icons)
            context.OMSetDepthStencilState(Some(&self.dss), 0);
            context.OMSetBlendState(&self.shadow_blend, Some(&blend_factor), 0xffffffff);
            self.draw_prims(context, &prepared, &singles, &groups, &frame, view_proj, None);

            // pass 3: wire catenaries
            self.draw_wires(context, &wire_groups, &frame, view_proj);

            saved.restore(context);
        }
    }
}

// seconds since the first frame (drives the emissive flicker phase)
// the accumulator's lightning prim (mat9) — only draws while the activity
// rate says it actually charges/discharges
fn is_arc_prim(key: &str, prim: &GpuPrim) -> bool {
    key.contains("accumulator") && prim.mat_name == "mat9"
}

// per-prim lit material slots (mat/emissive/pbr), shared by the instanced
// and single-draw paths of draw_prims
fn fill_lit_material(data: &mut ModelCB, prim: &GpuPrim, frame: &FrameCB, key: &str, arc: bool) {
    // lamps are off during the day — their dome emissive (and nothing
    // else's) fades in with the night
    let mut emis_mul = if key.contains("small-lamp") {
        frame.emissive_mul * frame.night
    } else {
        frame.emissive_mul
    };
    // arcs: tame the authored intensity so the blue tint survives the
    // tonemap instead of clipping to white
    if arc {
        emis_mul *= 0.45;
    }
    data.mat = [
        prim.metallic,
        (prim.roughness * frame.rough_mul).clamp(0.02, 1.0),
        emis_mul,
        frame.night,
    ];
    data.emissive = [
        prim.emissive[0],
        prim.emissive[1],
        prim.emissive[2],
        if prim.emissive_srv.is_some() { 1.0 } else { 0.0 },
    ];
    let flag = |b: bool| if b { 1.0 } else { 0.0 };
    data.pbr = [
        flag(prim.normal_srv.is_some()),
        flag(prim.mr_srv.is_some()),
        flag(prim.mr_has_ao),
        frame.base.pbr[3], // flicker time
    ];
}

fn seconds_running() -> f32 {
    use std::sync::OnceLock;
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f32()
}

// frame-constant lighting, cloned into every prim's cbuffer. sky/ground
// colors are settings; strengths/night/exposure are live tuning knobs
fn frame_constants(cam_eye: Vec3, sun: Vec3) -> FrameCB {
    // hold ALT: models go translucent so the vanilla alt-mode overlay
    // (drawn beneath our pass) shows through
    let alpha = {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_MENU};
        if unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } as u16 & 0x8000 != 0 { 0.4 } else { 1.0 }
    };
    // NIGHT knob < 0 = auto: follow the darkness the game computed this frame
    let night = crate::hooks::daynight::night_factor();
    // sky/ground ambient colors shift toward moonlight as night falls
    let ncol = settings::NIGHT_COLOR;
    let mix = |c: [f32; 3], t: f32| {
        [
            c[0] * (1.0 - night) + ncol[0] * night * t,
            c[1] * (1.0 - night) + ncol[1] * night * t,
            c[2] * (1.0 - night) + ncol[2] * night * t,
        ]
    };
    let sky = mix(settings::SKY_AMBIENT, 1.0);
    let ground = mix(settings::GROUND_AMBIENT, 0.6);
    // golden hour: bend the sun warm-orange while darkness is mid-transition
    // (4n(1-n) peaks at night=0.5, zero at full day/night)
    let dusk = 4.0 * night * (1.0 - night);
    let dc = settings::DUSK_SUN_COLOR;
    let sun_c: [f32; 3] = std::array::from_fn(|i| {
        settings::SUN_COLOR[i] + (dc[i] - settings::SUN_COLOR[i]) * dusk
    });
    FrameCB {
        base: ModelCB {
            mvp: [0.0; 16],
            model: [0.0; 16],
            base_color: [0.0; 4],
            opts: [0.0; 4],
            light: [sun.x, sun.y, sun.z, 0.0],
            misc: [0.0; 4],
            clipv: [0.0; 4],
            mat: [0.0; 4],
            emissive: [0.0; 4],
            cam: [cam_eye.x, cam_eye.y, cam_eye.z, tuning::LIGHT_EXPOSURE.get()],
            sky: [sky[0], sky[1], sky[2], tuning::LIGHT_AMBIENT.get()],
            ground: [ground[0], ground[1], ground[2], tuning::LIGHT_RIM.get()],
            sun: [sun_c[0], sun_c[1], sun_c[2], tuning::LIGHT_SUN.get()],
            params: [
                tuning::NIGHT_AMBIENT.get(),
                tuning::LIGHT_SPEC.get(),
                tuning::LIGHT_REFLECT.get(),
                tuning::LIGHT_FLICKER.get(),
            ],
            // w = wall-clock seconds for the emissive flicker
            pbr: [0.0, 0.0, 0.0, seconds_running()],
            clipy: [0.0; 4],
            lamps: [[0.0; 4]; MAX_LAMPS],
            lamp_meta: [0.0; 4],
        },
        alpha,
        night,
        rough_mul: tuning::LIGHT_ROUGH.get(),
        emissive_mul: tuning::LIGHT_EMISSIVE.get(),
    }
}

// 3d wire catenaries as raw vertices, grouped by color (one draw per color);
// built in plane space so world = identity
fn build_wire_verts(
    wires: &[crate::entities::WireDraw],
    view: &ViewMap,
) -> HashMap<[u32; 4], Vec<f32>> {
    let mut groups: HashMap<[u32; 4], Vec<f32>> = HashMap::new();
    let ttp = view.tile_to_plane;
    let hscale = tuning::WIRE_HEIGHT_SCALE.get();
    let hadd = tuning::WIRE_HEIGHT_ADD.get();
    let pole_add = tuning::WIRE_POLE_ADD.get();
    let yshift = tuning::WIRE_SHIFT_Y.get();
    let sag = tuning::WIRE_SAG.get();
    let half_w = tuning::WIRE_WIDTH.get().max(0.005) * ttp * 0.5;
    let poles = crate::entities::electric_pole_cells();
    // extra tiles of height at an endpoint that sits on an electric pole
    let end_add = |x: f32, y: f32| {
        let cx = x.floor() as i32;
        let cy = y.floor() as i32;
        for dx in -1..=1 {
            for dy in -1..=1 {
                if poles.contains(&(cx + dx, cy + dy)) {
                    return pole_add;
                }
            }
        }
        0.0
    };
    let mut total_verts = 0usize;
    for w in wires {
        let h1 = w.h * hscale + hadd + end_add(w.x1, w.y1);
        let h2 = w.h * hscale + hadd + end_add(w.x2, w.y2);
        // endpoints come pre-shifted north by the render height
        let (u1, v1) = view.uv(w.x1, w.y1 + h1 * yshift);
        let (u2, v2) = view.uv(w.x2, w.y2 + h2 * yshift);
        if !ViewMap::on_screen(u1, v1) && !ViewMap::on_screen(u2, v2) {
            continue;
        }
        if total_verts + WIRE_SEGS * 12 > WIRE_MAX_VERTS {
            break;
        }
        total_verts += WIRE_SEGS * 12;
        let (ax, az) = view.plane(u1, v1);
        let (bx, bz) = view.plane(u2, v2);
        let a = Vec3::new(ax, h1 * ttp, az);
        let b = Vec3::new(bx, h2 * ttp, bz);
        let dir = (b - a).with_y(0.0).normalize_or_zero();
        let perp = Vec3::new(-dir.z, 0.0, dir.x) * half_w;
        let up = Vec3::Y * half_w;
        let at = |t: f32| {
            let mut p = a.lerp(b, t);
            p.y -= sag * ttp * 4.0 * t * (1.0 - t); // parabolic hang
            p
        };
        let verts = groups.entry(w.color.map(f32::to_bits)).or_default();
        // two crossed ribbons per segment; only pos + normal are meaningful,
        // the rest of the 22-float vertex stays zero
        let mut quad = |p0: Vec3, p1: Vec3, off: Vec3, n: Vec3| {
            let c = [p0 - off, p0 + off, p1 + off, p1 - off];
            for i in [0usize, 1, 2, 0, 2, 3] {
                let p = c[i];
                let mut vert = [0.0f32; crate::gltf_model::VERTEX_FLOATS];
                vert[..6].copy_from_slice(&[p.x, p.y, p.z, n.x, n.y, n.z]);
                verts.extend_from_slice(&vert);
            }
        };
        for s in 0..WIRE_SEGS {
            let (p0, p1) = (at(s as f32 / WIRE_SEGS as f32), at((s + 1) as f32 / WIRE_SEGS as f32));
            quad(p0, p1, up, perp.normalize_or_zero()); // vertical ribbon
            quad(p0, p1, perp, Vec3::Y); // horizontal ribbon
        }
    }
    groups
}

// --- saved gpu state -------------------------------------------------------------------

// everything the model pass touches, restored exactly as the game left it
struct SavedState {
    rtvs: [Option<ID3D11RenderTargetView>; 1],
    dsv: Option<ID3D11DepthStencilView>,
    viewports: [D3D11_VIEWPORT; 1],
    num_vp: u32,
    rs: Option<ID3D11RasterizerState>,
    dss: Option<ID3D11DepthStencilState>,
    stencil_ref: u32,
    blend: Option<ID3D11BlendState>,
    blend_factor: [f32; 4],
    blend_mask: u32,
    topology: windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY,
    layout: Option<ID3D11InputLayout>,
    vb: [Option<ID3D11Buffer>; 1],
    vb_stride: u32,
    vb_offset: u32,
    ib: Option<ID3D11Buffer>,
    ib_fmt: DXGI_FORMAT,
    ib_offset: u32,
    vs: Option<ID3D11VertexShader>,
    ps: Option<ID3D11PixelShader>,
    vs_cb: [Option<ID3D11Buffer>; 3],
    ps_cb: [Option<ID3D11Buffer>; 1],
    samplers: [Option<ID3D11SamplerState>; 1],
    srvs: [Option<ID3D11ShaderResourceView>; 1],
    vs_srvs: [Option<ID3D11ShaderResourceView>; 1], // slot 4: instance matrices
}

impl SavedState {
    unsafe fn save(context: &ID3D11DeviceContext) -> Self {
        unsafe {
            let mut s = SavedState {
                rtvs: [None],
                dsv: None,
                viewports: [D3D11_VIEWPORT::default(); 1],
                num_vp: 1,
                rs: context.RSGetState().ok(),
                dss: None,
                stencil_ref: 0,
                blend: None,
                blend_factor: [0.0; 4],
                blend_mask: 0,
                topology: context.IAGetPrimitiveTopology(),
                layout: context.IAGetInputLayout().ok(),
                vb: [None],
                vb_stride: 0,
                vb_offset: 0,
                ib: None,
                ib_fmt: DXGI_FORMAT_UNKNOWN,
                ib_offset: 0,
                vs: None,
                ps: None,
                vs_cb: [None, None, None],
                ps_cb: [None],
                samplers: [None],
                srvs: [None],
                vs_srvs: [None],
            };
            context.OMGetRenderTargets(Some(&mut s.rtvs), Some(&mut s.dsv));
            context.RSGetViewports(&mut s.num_vp, Some(s.viewports.as_mut_ptr()));
            context.OMGetDepthStencilState(Some(&mut s.dss), Some(&mut s.stencil_ref));
            context.OMGetBlendState(
                Some(&mut s.blend),
                Some(&mut s.blend_factor),
                Some(&mut s.blend_mask),
            );
            context.IAGetVertexBuffers(
                0,
                1,
                Some(s.vb.as_mut_ptr()),
                Some(&mut s.vb_stride),
                Some(&mut s.vb_offset),
            );
            context.IAGetIndexBuffer(Some(&mut s.ib), Some(&mut s.ib_fmt), Some(&mut s.ib_offset));
            context.VSGetShader(&mut s.vs, None, None);
            context.PSGetShader(&mut s.ps, None, None);
            context.VSGetConstantBuffers(0, Some(&mut s.vs_cb));
            context.PSGetConstantBuffers(0, Some(&mut s.ps_cb));
            context.PSGetSamplers(0, Some(&mut s.samplers));
            context.PSGetShaderResources(0, Some(&mut s.srvs));
            context.VSGetShaderResources(4, Some(&mut s.vs_srvs));
            s
        }
    }

    unsafe fn restore(&self, context: &ID3D11DeviceContext) {
        unsafe {
            // slots 1-3 (our emissive/normal/mr textures) weren't used by the
            // game: clear them
            context.PSSetShaderResources(0, Some(&[self.srvs[0].clone(), None, None, None]));
            context.VSSetShaderResources(4, Some(&self.vs_srvs));
            context.PSSetSamplers(0, Some(&self.samplers));
            context.VSSetConstantBuffers(0, Some(&self.vs_cb));
            context.PSSetConstantBuffers(0, Some(&self.ps_cb));
            context.VSSetShader(self.vs.as_ref(), None);
            context.PSSetShader(self.ps.as_ref(), None);
            context.IASetInputLayout(self.layout.as_ref());
            context.IASetPrimitiveTopology(self.topology);
            context.IASetVertexBuffers(
                0,
                1,
                Some(self.vb.as_ptr()),
                Some(&self.vb_stride),
                Some(&self.vb_offset),
            );
            context.IASetIndexBuffer(self.ib.as_ref(), self.ib_fmt, self.ib_offset);
            context.OMSetRenderTargets(Some(&self.rtvs), self.dsv.as_ref());
            context.OMSetDepthStencilState(self.dss.as_ref(), self.stencil_ref);
            context.OMSetBlendState(self.blend.as_ref(), Some(&self.blend_factor), self.blend_mask);
            if self.num_vp > 0 {
                context.RSSetViewports(Some(&self.viewports[..self.num_vp as usize]));
            }
            context.RSSetState(self.rs.as_ref());
        }
    }
}

// --- d3d helpers -----------------------------------------------------------------------

fn vtx(
    name: windows::core::PCSTR,
    index: u32,
    format: DXGI_FORMAT,
    offset: u32,
) -> D3D11_INPUT_ELEMENT_DESC {
    D3D11_INPUT_ELEMENT_DESC {
        SemanticName: name,
        SemanticIndex: index,
        Format: format,
        InputSlot: 0,
        AlignedByteOffset: offset,
        InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
        InstanceDataStepRate: 0,
    }
}

// cpu-writable buffer (constant or vertex)
fn dynamic_buffer(device: &ID3D11Device, bytes: usize, bind: D3D11_BIND_FLAG) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: bytes as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    unsafe {
        let mut buf = None;
        device.CreateBuffer(&desc, None, Some(&mut buf))?;
        buf.context("CreateBuffer dynamic")
    }
}

// map-discard, copy, unmap
unsafe fn upload_raw(context: &ID3D11DeviceContext, buf: &ID3D11Buffer, ptr: *const u8, len: usize) {
    unsafe {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if context.Map(buf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).is_ok() {
            std::ptr::copy_nonoverlapping(ptr, mapped.pData as *mut u8, len);
            context.Unmap(buf, 0);
        }
    }
}

unsafe fn upload<T: Copy>(context: &ID3D11DeviceContext, buf: &ID3D11Buffer, data: &T) {
    unsafe { upload_raw(context, buf, data as *const T as *const u8, std::mem::size_of::<T>()) };
}

// cpu box-filter mip chain (level 0 = the source). without mips, minified
// sampling picks near-random texels — a zoomed-out belt field full of items
// dissolved into shimmering color confetti
fn mip_chain(w0: u32, h0: u32, base: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let (mut w, mut h) = (w0, h0);
    let mut levels = vec![(w, base.to_vec())];
    while w > 1 || h > 1 {
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let prev = &levels.last().unwrap().1;
        let mut next = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                // clamp the 2x2 source window (odd sizes)
                let (x0, y0) = ((x * 2).min(w - 1), (y * 2).min(h - 1));
                let (x1, y1) = ((x * 2 + 1).min(w - 1), (y * 2 + 1).min(h - 1));
                for c in 0..4usize {
                    let s = |sx: u32, sy: u32| prev[((sy * w + sx) * 4) as usize + c] as u32;
                    next[((y * nw + x) * 4) as usize + c] =
                        ((s(x0, y0) + s(x1, y0) + s(x0, y1) + s(x1, y1) + 2) / 4) as u8;
                }
            }
        }
        levels.push((nw, next));
        (w, h) = (nw, nh);
    }
    levels
}

// one immutable rgba texture + its shader resource view (albedo or emissive),
// full mip chain included
fn make_texture_srv(
    device: &ID3D11Device,
    t: &crate::gltf_model::TexData,
) -> Result<ID3D11ShaderResourceView> {
    let levels = mip_chain(t.width, t.height, &t.rgba);
    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: t.width,
        Height: t.height,
        MipLevels: levels.len() as u32,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let init: Vec<D3D11_SUBRESOURCE_DATA> = levels
        .iter()
        .map(|(w, data)| D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr() as *const _,
            SysMemPitch: w * 4,
            ..Default::default()
        })
        .collect();
    unsafe {
        let mut tx = None;
        device.CreateTexture2D(&tex_desc, Some(init.as_ptr()), Some(&mut tx))?;
        let tex = tx.context("CreateTexture2D model tex")?;
        let mut s = None;
        device.CreateShaderResourceView(&tex, None, Some(&mut s))?;
        s.context("CreateSRV model tex")
    }
}

fn upload_model(device: &ID3D11Device, model: Arc<ModelData>) -> Result<GpuModel> {
    let mut prims = Vec::with_capacity(model.prims.len());
    for p in &model.prims {
        // take the cpu-side payload — frees the ram once uploaded
        let Some(src) = p.src.lock().unwrap().take() else {
            anyhow::bail!("prim source already consumed (device reset?)");
        };
        let vb_desc = D3D11_BUFFER_DESC {
            ByteWidth: (src.vertices.len() * 4) as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            ..Default::default()
        };
        let vb_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: src.vertices.as_ptr() as *const _,
            ..Default::default()
        };
        let vb = unsafe {
            let mut b = None;
            device.CreateBuffer(&vb_desc, Some(&vb_data), Some(&mut b))?;
            b.context("CreateBuffer model VB")?
        };
        let ib_desc = D3D11_BUFFER_DESC {
            ByteWidth: (src.indices.len() * 4) as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_INDEX_BUFFER.0 as u32,
            ..Default::default()
        };
        let ib_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: src.indices.as_ptr() as *const _,
            ..Default::default()
        };
        let ib = unsafe {
            let mut b = None;
            device.CreateBuffer(&ib_desc, Some(&ib_data), Some(&mut b))?;
            b.context("CreateBuffer model IB")?
        };
        if let Some(s) = p.skin {
            if model.skins[s].joints.len() > MAX_BONES {
                log::warn!(
                    "[model] skin has {} joints (max {MAX_BONES}) — animation will glitch",
                    model.skins[s].joints.len()
                );
            }
        }
        let make = |t: &Option<crate::gltf_model::TexData>| -> Result<Option<ID3D11ShaderResourceView>> {
            t.as_ref().map(|t| make_texture_srv(device, t)).transpose()
        };
        let srv = make(&src.tex)?;
        let emissive_srv = make(&src.emissive_tex)?;
        let normal_srv = make(&src.normal_tex)?;
        let mr_srv = make(&src.mr_tex)?;
        prims.push(GpuPrim {
            node: p.node,
            skin: p.skin,
            vb,
            ib,
            index_count: p.index_count,
            base_color: p.base_color,
            metallic: p.metallic,
            roughness: p.roughness,
            emissive: p.emissive,
            srv,
            emissive_srv,
            normal_srv,
            mr_srv,
            mr_has_ao: p.mr_has_ao,
            tintable: p.tintable,
            mat_name: p.mat_name.clone(),
        });
    }
    let ext = model.aabb_max - model.aabb_min;
    let rest_worlds = model.node_worlds(0.0);
    let rest_skins: Vec<Vec<Mat4>> = (0..model.skins.len())
        .map(|s| model.skin_matrices(s, &rest_worlds))
        .collect();
    let track = model.track_path.as_ref().map(|tp| {
        let mut cb = Box::new(TrackCB {
            pts: [[0.0; 4]; MAX_TRACK_PTS],
            meta: [tp.points.len() as f32, tp.total, 0.0, 0.0],
            lat: [tp.lateral.x, tp.lateral.y, tp.lateral.z, 0.0],
        });
        for (i, p) in tp.points.iter().enumerate().take(MAX_TRACK_PTS) {
            cb.pts[i] = [p.x, p.y, p.z, tp.cumlen[i]];
        }
        cb
    });
    Ok(GpuModel {
        extent_xz: ext.x.max(ext.z),
        rest_worlds: Arc::new(rest_worlds),
        rest_skins: Arc::new(rest_skins),
        track,
        prims,
        data: model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // the shaders compile at runtime; a HLSL error would silently disable all
    // models in-game. compile them here (no gpu device needed) so a bad edit
    // fails the build instead
    #[test]
    fn shaders_compile() {
        crate::warp::compile_shader(&format!("{MODEL_HLSL}{MODEL_VS}"), "vs_5_0")
            .expect("vertex shader must compile");
        crate::warp::compile_shader(&format!("{MODEL_HLSL}{MODEL_PS}"), "ps_5_0")
            .expect("pixel shader must compile");
    }
}

// loads one glb: node hierarchy, mesh primitives (node-local vertices), the
// base-color textures, and the animation. node TRS channels (translation /
// rotation / scale) are kept per node and sampled at draw time, so the c4d
// exports play back properly. skinned meshes (the c4d rigs) carry joint
// indices/weights per vertex and a skin (joints + inverse bind matrices).
// morph targets (first target) still supported.
//
// when a file contains several animations (c4d takes), the one with the most
// channels wins (tie: longest).

use glam::{Mat3, Mat4, Quat, Vec3};
use std::path::Path;

pub struct TexData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

// one gpu-ready primitive, interleaved:
// pos(3) normal(3) uv(2) morph_dpos(3) morph_dnormal(3) joints(4) weights(4)
// = 22 floats
pub const VERTEX_FLOATS: usize = 22;

// geometry + texture payload, TAKEN by the gpu upload so the ram is freed
// (keeping decoded rgba for 100+ models pinned gigabytes and fed the page
// file on slow disks)
pub struct PrimSrc {
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    pub tex: Option<TexData>,
    // glowing parts (lamp lenses, accumulator top, furnace fire) — sampled and
    // added on top of the lit color so they stay bright at night
    pub emissive_tex: Option<TexData>,
    // tangent-space normal map (t2) — per-pixel surface detail
    pub normal_tex: Option<TexData>,
    // metallic-roughness map (t3): g = roughness, b = metallic (gltf spec);
    // r = ambient occlusion when the occlusion texture shares this image (ORM)
    pub mr_tex: Option<TexData>,
}

pub struct PrimData {
    pub node: usize,         // index into ModelData::nodes
    pub skin: Option<usize>, // index into ModelData::skins
    pub base_color: [f32; 4],
    // pbr factors straight from the gltf material (FUE5 authored these):
    // metallic ~0 for painted casings, roughness ~0.55; emissive rgb on the
    // few glowing materials. the shader turns these into specular + glow
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    // the mr texture is ORM-packed: its red channel is baked ambient occlusion
    pub mr_has_ao: bool,
    // instance tint (player color) applies to this prim — everything except
    // "joint" materials, so the mixamo bot's dark joints stay dark
    pub tintable: bool,
    // gltf material name — the renderer keys per-material rules on it
    // (accumulator arcs only draw while charging)
    pub mat_name: String,
    pub index_count: u32,
    pub src: std::sync::Mutex<Option<PrimSrc>>,
}

pub struct SkinData {
    pub joints: Vec<usize>, // indices into ModelData::nodes
    pub ibms: Vec<Mat4>,    // inverse bind matrices, same order
}

// linear keyframe track
pub struct Keys<T: Copy> {
    pub times: Vec<f32>,
    pub values: Vec<T>,
}

impl<T: Copy> Keys<T> {
    fn sample_with(&self, t: f32, lerp: impl Fn(T, T, f32) -> T) -> Option<T> {
        if self.times.is_empty() {
            return None;
        }
        if t <= self.times[0] {
            return Some(self.values[0]);
        }
        if t >= *self.times.last().unwrap() {
            return Some(*self.values.last().unwrap());
        }
        let i = self.times.partition_point(|&x| x <= t).min(self.times.len() - 1);
        let (t0, t1) = (self.times[i - 1], self.times[i]);
        let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
        Some(lerp(self.values[i - 1], self.values[i], f))
    }
}

impl Keys<Vec3> {
    pub fn sample(&self, t: f32, rest: Vec3) -> Vec3 {
        self.sample_with(t, |a, b, f| a.lerp(b, f)).unwrap_or(rest)
    }
}

impl Keys<Quat> {
    pub fn sample(&self, t: f32, rest: Quat) -> Quat {
        self.sample_with(t, |a, b, f| a.slerp(b, f)).unwrap_or(rest)
    }
}

impl Keys<f32> {
    pub fn sample(&self, t: f32) -> f32 {
        self.sample_with(t, |a, b, f| a + (b - a) * f).unwrap_or(0.0)
    }
}

// one gltf node: rest TRS + optional animation tracks
pub struct NodeData {
    pub parent: Option<usize>,
    pub t: Vec3,
    pub r: Quat,
    pub s: Vec3,
    pub t_anim: Option<Keys<Vec3>>,
    pub r_anim: Option<Keys<Quat>>,
    pub s_anim: Option<Keys<Vec3>>,
    // named pose targets (tank): the turret hull node spins with the gun,
    // the tracks node scrolls while driving, wheel nodes spin on their axle
    pub is_turret: bool,
    pub is_track: bool,
    pub is_wheel: bool,
    pub wheel_radius: f32, // model units, from the node's mesh (0 = no mesh)
    // spidertron rig role from the node name (Body / LegN_Upper/Lower/Foot)
    pub spider: SpiderRole,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpiderRole {
    None,
    Body,            // torso node — yawed by the head orientation
    Leg(u8, LegSeg), // leg 0..7, segment
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LegSeg {
    Upper, // hip -> knee
    Lower, // knee -> foot
    Foot,  // foot-tip marker
}

// one leg's rest geometry in model space
#[derive(Clone, Copy)]
pub struct SpiderLegRig {
    pub upper: usize,
    pub lower: usize,
    pub root: usize, // parent of upper
    pub hip: Vec3,
    pub knee: Vec3,
    pub foot: Vec3,
    pub l1: f32, // hip->knee
    pub l2: f32, // knee->foot
    pub pole: Vec3, // rest bend direction
}

// torso node + 8 legs; only on the spidertron model
#[derive(Clone)]
pub struct SpiderRig {
    pub body: Option<usize>,
    pub legs: Vec<SpiderLegRig>,
}

// live per-instance overrides applied on top of the sampled animation
#[derive(Clone, Default)]
pub struct Pose {
    pub turret_yaw: f32,    // radians around the turret node's local up
    pub track_offset: f32,  // model units along the track node's local z
    pub wheel_advance: f32, // model units driven -> wheel angle via radius
    // per-node local override (spidertron torso + IK'd legs); len == nodes
    pub node_locals: Option<std::sync::Arc<Vec<Option<Mat4>>>>,
}

// the chain loop midline (track_path.json next to the glb): chain vertices
// flow along this closed polyline in the vertex shader instead of the whole
// node translating (which wobbled at the sprocket ends)
pub struct TrackPath {
    pub points: Vec<Vec3>, // ordered around the closed loop, node-local space
    pub cumlen: Vec<f32>,  // arc length at each point
    pub total: f32,
    pub lateral: Vec3, // loop plane normal (the axis toward the other band)
}

impl NodeData {
    fn local(&self, t: f32) -> Mat4 {
        let tr = self.t_anim.as_ref().map(|k| k.sample(t, self.t)).unwrap_or(self.t);
        let ro = self.r_anim.as_ref().map(|k| k.sample(t, self.r)).unwrap_or(self.r);
        let sc = self.s_anim.as_ref().map(|k| k.sample(t, self.s)).unwrap_or(self.s);
        Mat4::from_scale_rotation_translation(sc, ro, tr)
    }
}

pub struct ModelData {
    pub nodes: Vec<NodeData>, // parents always precede children
    pub skins: Vec<SkinData>,
    pub prims: Vec<PrimData>,
    pub weight_anim: Option<Keys<f32>>,
    pub duration: f32,
    // rest-pose bounds in world (z-flipped) space
    pub aabb_min: Vec3,
    pub aabb_max: Vec3,
    // any turret/track/wheel node present — only then is the posed path worth it
    pub has_pose_nodes: bool,
    pub track_path: Option<TrackPath>,
    pub spider: Option<SpiderRig>, // torso + 8 IK legs; None for other models
}

fn compute_node_worlds(nodes: &[NodeData], t: f32, pose: &Pose) -> Vec<Mat4> {
    let mut out = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        // spidertron: a full local override wins over the flag-based nudges
        if let Some(ov) = pose.node_locals.as_ref().and_then(|l| l[i]) {
            let world = match n.parent {
                Some(p) => out[p] * ov,
                None => ov,
            };
            out.push(world);
            continue;
        }
        let mut local = n.local(t);
        // post-multiplied: rotates/moves in the node's own object frame,
        // same as transforming the object locally in blender
        if pose.turret_yaw != 0.0 && n.is_turret {
            local *= Mat4::from_quat(Quat::from_rotation_y(pose.turret_yaw));
        }
        if pose.track_offset != 0.0 && n.is_track {
            local *= Mat4::from_translation(Vec3::new(0.0, 0.0, pose.track_offset));
        }
        if pose.wheel_advance != 0.0 && n.is_wheel && n.wheel_radius > 1e-3 {
            local *= Mat4::from_quat(Quat::from_rotation_y(pose.wheel_advance / n.wheel_radius));
        }
        let world = match n.parent {
            Some(p) => out[p] * local,
            None => local,
        };
        out.push(world);
    }
    out
}

fn compute_skin_matrices(skin: &SkinData, node_worlds: &[Mat4]) -> Vec<Mat4> {
    skin.joints
        .iter()
        .zip(&skin.ibms)
        .map(|(&j, ibm)| node_worlds[j] * *ibm)
        .collect()
}

impl ModelData {
    // world matrix per node at animation time t (parents precede children)
    pub fn node_worlds(&self, t: f32) -> Vec<Mat4> {
        compute_node_worlds(&self.nodes, t, &Pose::default())
    }

    // node worlds with live pose overrides (turret spin, track scroll)
    pub fn node_worlds_posed(&self, t: f32, pose: &Pose) -> Vec<Mat4> {
        compute_node_worlds(&self.nodes, t, pose)
    }

    // model-space skinning matrices for one skin, given this frame's node worlds
    pub fn skin_matrices(&self, skin: usize, node_worlds: &[Mat4]) -> Vec<Mat4> {
        compute_skin_matrices(&self.skins[skin], node_worlds)
    }
}

pub fn load(path: &Path) -> anyhow::Result<ModelData> {
    let (doc, buffers, images) = gltf::import(path)?;

    // flatten the node tree, parents first
    let scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .ok_or_else(|| anyhow::anyhow!("no scene"))?;
    let mut nodes: Vec<NodeData> = Vec::new();
    let mut gltf_to_local = vec![usize::MAX; doc.nodes().count()];
    let mut stack: Vec<(gltf::Node, Option<usize>)> =
        scene.nodes().map(|n| (n, None)).collect();
    // meshes attached to nodes, resolved after the node list is built
    let mut node_meshes: Vec<(usize, gltf::Mesh, Option<usize>)> = Vec::new();
    while let Some((node, parent)) = stack.pop() {
        let (t, r, s) = node.transform().decomposed();
        let idx = nodes.len();
        gltf_to_local[node.index()] = idx;
        let lname = node.name().map(|n| n.to_lowercase()).unwrap_or_default();
        nodes.push(NodeData {
            parent,
            t: Vec3::from(t),
            r: Quat::from_array(r),
            s: Vec3::from(s),
            t_anim: None,
            r_anim: None,
            s_anim: None,
            // "turrethull" not "turret": barrel children of the hull node
            // inherit its world and must not rotate twice
            is_turret: lname.contains("turrethull"),
            is_track: lname.contains("track"),
            // road wheels only — "wheelsframes"/"wheelssuspensions" stay put
            is_wheel: lname == "wheels" || lname.starts_with("wheels."),
            wheel_radius: 0.0,
            spider: parse_spider_role(&lname),
        });
        // "scorch" nodes are baked ground-burn decals; their alpha map is
        // lost in the jpg textures so they'd render as solid black slabs
        let is_scorch = node
            .name()
            .is_some_and(|n| n.to_lowercase().contains("scorch"));
        if let Some(mesh) = node.mesh() {
            if !is_scorch {
                node_meshes.push((idx, mesh, node.skin().map(|s| s.index())));
            }
        }
        for child in node.children() {
            stack.push((child, Some(idx)));
        }
    }
    // stack order put children before later siblings but parents always
    // precede their children, which is all node_worlds() needs

    // pick the animation with the most channels (tie: longest duration)
    let mut best: Option<gltf::Animation> = None;
    let mut best_score = (0usize, 0.0f32);
    for a in doc.animations() {
        let n = a.channels().count();
        let dur = anim_duration(&a, &buffers);
        if n > best_score.0 || (n == best_score.0 && dur > best_score.1) {
            best_score = (n, dur);
            best = Some(a);
        }
    }

    let mut weight_anim: Option<Keys<f32>> = None;
    let mut duration = 0.0f32;
    if let Some(a) = &best {
        for ch in a.channels() {
            let reader = ch.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let times: Vec<f32> = match reader.read_inputs() {
                Some(t) => t.collect(),
                None => continue,
            };
            if times.is_empty() {
                continue;
            }
            duration = duration.max(*times.last().unwrap());
            let node_idx = gltf_to_local[ch.target().node().index()];
            if node_idx == usize::MAX {
                continue;
            }
            use gltf::animation::util::ReadOutputs as O;
            match reader.read_outputs() {
                Some(O::Translations(v)) => {
                    let values: Vec<Vec3> = v.map(Vec3::from).collect();
                    if values.len() == times.len() {
                        nodes[node_idx].t_anim = Some(Keys { times, values });
                    }
                }
                Some(O::Rotations(v)) => {
                    let values: Vec<Quat> =
                        v.into_f32().map(Quat::from_array).collect();
                    if values.len() == times.len() {
                        nodes[node_idx].r_anim = Some(Keys { times, values });
                    }
                }
                Some(O::Scales(v)) => {
                    let values: Vec<Vec3> = v.map(Vec3::from).collect();
                    if values.len() == times.len() {
                        nodes[node_idx].s_anim = Some(Keys { times, values });
                    }
                }
                Some(O::MorphTargetWeights(w)) if weight_anim.is_none() => {
                    // flattened keys x targets — keep the FIRST target's track
                    let all: Vec<f32> = w.into_f32().collect();
                    let per_key = all.len() / times.len().max(1);
                    if per_key >= 1 && all.len() == per_key * times.len() {
                        let values: Vec<f32> =
                            (0..times.len()).map(|i| all[i * per_key]).collect();
                        weight_anim = Some(Keys { times, values });
                    }
                }
                _ => {}
            }
        }
    }

    // skins: joints (as local node indices) + inverse bind matrices
    let mut skins: Vec<SkinData> = Vec::new();
    let mut skin_to_local = vec![usize::MAX; doc.skins().count()];
    for skin in doc.skins() {
        let joints: Vec<usize> =
            skin.joints().map(|n| gltf_to_local[n.index()]).collect();
        if joints.iter().any(|&j| j == usize::MAX) {
            continue; // joints outside the scene — skip the skin
        }
        let reader = skin.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
        let ibms: Vec<Mat4> = reader
            .read_inverse_bind_matrices()
            .map(|it| it.map(|m| Mat4::from_cols_array_2d(&m)).collect())
            .unwrap_or_else(|| vec![Mat4::IDENTITY; joints.len()]);
        if ibms.len() != joints.len() {
            continue;
        }
        skin_to_local[skin.index()] = skins.len();
        skins.push(SkinData { joints, ibms });
    }

    // rest-pose transforms, for the bounds pass below
    let rest_worlds = compute_node_worlds(&nodes, 0.0, &Pose::default());
    let spider = build_spider_rig(&nodes, &rest_worlds);
    let rest_skin_mats: Vec<Vec<Mat4>> =
        skins.iter().map(|s| compute_skin_matrices(s, &rest_worlds)).collect();
    let flip = Vec3::new(1.0, 1.0, -1.0);
    let mut aabb_min = Vec3::splat(f32::MAX);
    let mut aabb_max = Vec3::splat(f32::MIN);

    // nodes that never move: no animation channel and no live-pose flag on
    // the node or any ancestor — their world transform is the rest pose,
    // forever. parents precede children, so one forward pass suffices
    let mut node_static = vec![false; nodes.len()];
    for i in 0..nodes.len() {
        let n = &nodes[i];
        let still = n.t_anim.is_none()
            && n.r_anim.is_none()
            && n.s_anim.is_none()
            && !n.is_turret
            && !n.is_track
            && !n.is_wheel
            // spidertron torso + legs are posed at runtime, keep them un-merged
            && n.spider == SpiderRole::None;
        node_static[i] = still && n.parent.is_none_or(|p| node_static[p]);
    }

    // static prims (no skin, motionless node) merge into ONE prim per
    // material, node transforms baked into the vertices. the FUE5 exports
    // arrive heavily fragmented (rocket-silo: 1708 meshes, every bolt its own
    // node) and each prim costs 2 draw calls + a cbuffer upload per frame —
    // plus each decoded its own copy of the shared textures
    struct MergeGroup {
        base_color: [f32; 4],
        metallic: f32,
        roughness: f32,
        emissive: [f32; 3],
        mr_has_ao: bool,
        tintable: bool,
        mat_name: String,
        vertices: Vec<f32>,
        indices: Vec<u32>,
        tex: Option<TexData>,
        emissive_tex: Option<TexData>,
        normal_tex: Option<TexData>,
        mr_tex: Option<TexData>,
    }
    let mut groups: Vec<(Option<usize>, MergeGroup)> = Vec::new();
    let mut merged_prims = 0usize;

    // read the primitives, vertices stay node-local
    let mut prims: Vec<PrimData> = Vec::new();
    for (node_idx, mesh, skin_idx) in &node_meshes {
        let local_skin = skin_idx
            .and_then(|s| skin_to_local.get(s).copied())
            .filter(|&s| s != usize::MAX);
        for prim in mesh.primitives() {
            let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            let positions: Vec<[f32; 3]> = match reader.read_positions() {
                Some(p) => p.collect(),
                None => continue,
            };
            // wheel axle = node-local y, so the spin radius lives in xz
            if nodes[*node_idx].is_wheel {
                let r = positions
                    .iter()
                    .map(|p| (p[0] * p[0] + p[2] * p[2]).sqrt())
                    .fold(0.0f32, f32::max);
                nodes[*node_idx].wheel_radius = nodes[*node_idx].wheel_radius.max(r);
            }
            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|n| n.collect())
                .unwrap_or_else(|| vec![[0.0, 1.0, 0.0]; positions.len()]);
            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|t| t.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);
            let indices: Vec<u32> = reader
                .read_indices()
                .map(|i| i.into_u32().collect())
                .unwrap_or_else(|| (0..positions.len() as u32).collect());

            // first morph target's deltas (shape key)
            let mut dpos = vec![[0.0f32; 3]; positions.len()];
            let mut dnorm = vec![[0.0f32; 3]; positions.len()];
            for (ti, (p, n, _t)) in reader.read_morph_targets().enumerate() {
                if ti > 0 {
                    break;
                }
                if let Some(p) = p {
                    for (i, d) in p.enumerate().take(dpos.len()) {
                        dpos[i] = d;
                    }
                }
                if let Some(n) = n {
                    for (i, d) in n.enumerate().take(dnorm.len()) {
                        dnorm[i] = d;
                    }
                }
            }

            // joint indices + weights (skinned meshes only, zeros otherwise)
            let joints: Vec<[u16; 4]> = reader
                .read_joints(0)
                .map(|j| j.into_u16().collect())
                .unwrap_or_else(|| vec![[0; 4]; positions.len()]);
            let weights: Vec<[f32; 4]> = reader
                .read_weights(0)
                .map(|w| w.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0; 4]; positions.len()]);

            let mut vertices = Vec::with_capacity(positions.len() * VERTEX_FLOATS);
            for i in 0..positions.len() {
                vertices.extend_from_slice(&[
                    positions[i][0], positions[i][1], positions[i][2],
                    normals[i][0], normals[i][1], normals[i][2],
                    uvs[i][0], uvs[i][1],
                    dpos[i][0], dpos[i][1], dpos[i][2],
                    dnorm[i][0], dnorm[i][1], dnorm[i][2],
                    joints[i][0] as f32, joints[i][1] as f32,
                    joints[i][2] as f32, joints[i][3] as f32,
                    weights[i][0], weights[i][1], weights[i][2], weights[i][3],
                ]);
            }

            // rest-pose bounds in the same (z-flipped) space the renderer
            // uses; skinned prims ignore their node transform (gltf spec)
            let skin_mats = local_skin.map(|s| &rest_skin_mats[s]);
            let w = rest_worlds[*node_idx];
            for v in vertices.chunks_exact(VERTEX_FLOATS) {
                let pos = Vec3::new(v[0], v[1], v[2]);
                let pt = match skin_mats {
                    Some(mats) => {
                        let mut out = Vec3::ZERO;
                        for k in 0..4 {
                            let (j, wt) = (v[14 + k] as usize, v[18 + k]);
                            if wt > 0.0 && j < mats.len() {
                                out += mats[j].transform_point3(pos) * wt;
                            }
                        }
                        out
                    }
                    None => w.transform_point3(pos),
                } * flip;
                aabb_min = aabb_min.min(pt);
                aabb_max = aabb_max.max(pt);
            }

            let material = prim.material();
            let pbr = material.pbr_metallic_roughness();
            let base_color = pbr.base_color_factor();
            let metallic = pbr.metallic_factor();
            let roughness = pbr.roughness_factor();
            let emissive = material.emissive_factor();
            let mr_src_idx = pbr.metallic_roughness_texture().map(|i| i.texture().source().index());
            // blender/most exporters pack occlusion into the same image (ORM)
            let mr_has_ao = mr_src_idx.is_some()
                && material.occlusion_texture().map(|o| o.texture().source().index()) == mr_src_idx;
            let tintable = material
                .name()
                .is_none_or(|n| !n.to_lowercase().contains("joint"));
            let mat_name = material.name().unwrap_or("").to_string();
            // decoding is the expensive part: merged prims share their
            // material's textures, so it runs once per material there
            let decode_textures = || {
                let tex = pbr.base_color_texture().and_then(|info| {
                    let img = images.get(info.texture().source().index())?;
                    to_rgba(img)
                });
                let emissive_tex = material.emissive_texture().and_then(|info| {
                    let img = images.get(info.texture().source().index())?;
                    to_rgba(img)
                });
                let normal_tex = material.normal_texture().and_then(|info| {
                    let img = images.get(info.texture().source().index())?;
                    to_rgba(img)
                });
                let mr_tex = mr_src_idx.and_then(|i| to_rgba(images.get(i)?));
                (tex, emissive_tex, normal_tex, mr_tex)
            };

            if local_skin.is_none() && node_static[*node_idx] {
                // bake the (constant) node world into the geometry. morph
                // deltas are vectors (linear part only), normals use the
                // inverse-transpose to survive non-uniform scale
                let lin = Mat3::from_mat4(w);
                let mut nrm = lin.inverse().transpose();
                if !nrm.is_finite() {
                    nrm = lin;
                }
                for v in vertices.chunks_exact_mut(VERTEX_FLOATS) {
                    let p = w.transform_point3(Vec3::new(v[0], v[1], v[2]));
                    let n = (nrm * Vec3::new(v[3], v[4], v[5])).normalize_or_zero();
                    let dp = lin * Vec3::new(v[8], v[9], v[10]);
                    let dn = nrm * Vec3::new(v[11], v[12], v[13]);
                    v[..6].copy_from_slice(&[p.x, p.y, p.z, n.x, n.y, n.z]);
                    v[8..14].copy_from_slice(&[dp.x, dp.y, dp.z, dn.x, dn.y, dn.z]);
                }
                let mat_id = material.index();
                let gi = groups.iter().position(|(k, _)| *k == mat_id).unwrap_or_else(|| {
                    let (tex, emissive_tex, normal_tex, mr_tex) = decode_textures();
                    groups.push((
                        mat_id,
                        MergeGroup {
                            base_color,
                            metallic,
                            roughness,
                            emissive,
                            mr_has_ao,
                            tintable,
                            mat_name: mat_name.clone(),
                            vertices: Vec::new(),
                            indices: Vec::new(),
                            tex,
                            emissive_tex,
                            normal_tex,
                            mr_tex,
                        },
                    ));
                    groups.len() - 1
                });
                let g = &mut groups[gi].1;
                let base = (g.vertices.len() / VERTEX_FLOATS) as u32;
                g.vertices.append(&mut vertices);
                g.indices.extend(indices.iter().map(|i| i + base));
                merged_prims += 1;
                continue;
            }

            let (tex, emissive_tex, normal_tex, mr_tex) = decode_textures();
            prims.push(PrimData {
                node: *node_idx,
                skin: local_skin,
                base_color,
                metallic,
                roughness,
                emissive,
                mr_has_ao,
                tintable,
                mat_name,
                index_count: indices.len() as u32,
                src: std::sync::Mutex::new(Some(PrimSrc {
                    vertices,
                    indices,
                    tex,
                    emissive_tex,
                    normal_tex,
                    mr_tex,
                })),
            });
        }
    }
    if !groups.is_empty() {
        // all merged geometry hangs off one synthetic identity node
        let ident = nodes.len();
        nodes.push(NodeData {
            parent: None,
            t: Vec3::ZERO,
            r: Quat::IDENTITY,
            s: Vec3::ONE,
            t_anim: None,
            r_anim: None,
            s_anim: None,
            is_turret: false,
            is_track: false,
            is_wheel: false,
            wheel_radius: 0.0,
            spider: SpiderRole::None,
        });
        let n_groups = groups.len();
        for (_, g) in groups {
            prims.push(PrimData {
                node: ident,
                skin: None,
                base_color: g.base_color,
                metallic: g.metallic,
                roughness: g.roughness,
                emissive: g.emissive,
                mr_has_ao: g.mr_has_ao,
                tintable: g.tintable,
                mat_name: g.mat_name,
                index_count: g.indices.len() as u32,
                src: std::sync::Mutex::new(Some(PrimSrc {
                    vertices: g.vertices,
                    indices: g.indices,
                    tex: g.tex,
                    emissive_tex: g.emissive_tex,
                    normal_tex: g.normal_tex,
                    mr_tex: g.mr_tex,
                })),
            });
        }
        if merged_prims > n_groups {
            log::info!(
                "[model] {}: merged {merged_prims} static prims into {n_groups} (one per material)",
                path.display()
            );
        }
    }
    if prims.is_empty() {
        anyhow::bail!("no mesh primitives found");
    }
    if aabb_min.x > aabb_max.x {
        anyhow::bail!("empty bounds");
    }
    let has_pose_nodes =
        nodes.iter().any(|n| n.is_turret || n.is_track || n.is_wheel) || spider.is_some();
    let track_path = if nodes.iter().any(|n| n.is_track) { load_track_path(path) } else { None };
    Ok(ModelData {
        nodes,
        skins,
        prims,
        weight_anim,
        duration,
        aabb_min,
        aabb_max,
        has_pose_nodes,
        track_path,
        spider,
    })
}

// node name (already lowercased) -> rig role: body, legN_upper/lower/foot
fn parse_spider_role(lname: &str) -> SpiderRole {
    if lname == "body" {
        return SpiderRole::Body;
    }
    if let Some(rest) = lname.strip_prefix("leg") {
        // "0_upper" -> (0, Upper)
        if let Some((idx, seg)) = rest.split_once('_') {
            if let Ok(i) = idx.parse::<u8>() {
                let seg = match seg {
                    "upper" => Some(LegSeg::Upper),
                    "lower" => Some(LegSeg::Lower),
                    "foot" => Some(LegSeg::Foot),
                    _ => None,
                };
                if let Some(s) = seg {
                    if i < 8 {
                        return SpiderRole::Leg(i, s);
                    }
                }
            }
        }
    }
    SpiderRole::None
}

// build the rig from the tagged nodes + their rest world positions
fn build_spider_rig(nodes: &[NodeData], rest: &[Mat4]) -> Option<SpiderRig> {
    let mut body = None;
    let mut upper = [usize::MAX; 8];
    let mut lower = [usize::MAX; 8];
    let mut foot = [usize::MAX; 8];
    let mut any = false;
    for (i, n) in nodes.iter().enumerate() {
        match n.spider {
            SpiderRole::Body => body = Some(i),
            SpiderRole::Leg(l, LegSeg::Upper) => {
                upper[l as usize] = i;
                any = true;
            }
            SpiderRole::Leg(l, LegSeg::Lower) => lower[l as usize] = i,
            SpiderRole::Leg(l, LegSeg::Foot) => foot[l as usize] = i,
            SpiderRole::None => {}
        }
    }
    if !any {
        return None;
    }
    let pos = |idx: usize| rest[idx].w_axis.truncate();
    let mut legs = Vec::new();
    for l in 0..8 {
        let (u, lo, ft) = (upper[l], lower[l], foot[l]);
        if u == usize::MAX || lo == usize::MAX || ft == usize::MAX {
            continue;
        }
        let (hip, knee, foot_p) = (pos(u), pos(lo), pos(ft));
        let l1 = (knee - hip).length();
        let l2 = (foot_p - knee).length();
        if l1 < 1e-4 || l2 < 1e-4 {
            continue;
        }
        // bend direction: knee offset from the hip->foot line
        let axis = (foot_p - hip).normalize_or_zero();
        let pole = {
            let v = knee - hip;
            let perp = v - axis * v.dot(axis);
            perp.normalize_or_zero()
        };
        legs.push(SpiderLegRig {
            upper: u,
            lower: lo,
            root: nodes[u].parent.unwrap_or(u),
            hip,
            knee,
            foot: foot_p,
            l1,
            l2,
            pole,
        });
    }
    if legs.is_empty() {
        return None;
    }
    Some(SpiderRig { body, legs })
}

impl SpiderRig {
    // per-node local overrides for one instance: torso yaw + each leg's IK to
    // its model-space foot target (None = leave that leg at rest)
    pub fn pose_locals(
        &self,
        nodes: &[NodeData],
        rest: &[Mat4],
        body_yaw: f32,
        targets: &[Option<Vec3>; 8],
    ) -> Vec<Option<Mat4>> {
        let mut locals: Vec<Option<Mat4>> = vec![None; nodes.len()];
        // torso: yaw about the node's local up
        if let Some(b) = self.body {
            let parent = nodes[b].parent.map(|p| rest[p]).unwrap_or(Mat4::IDENTITY);
            let rest_local = parent.inverse() * rest[b];
            locals[b] = Some(rest_local * Mat4::from_quat(Quat::from_rotation_y(body_yaw)));
        }
        // the game's leg order != the model's Leg0..7, so match each leg to the
        // foot with the nearest azimuth (xz plane). walk legs in azimuth order
        // so the greedy match doesn't steal a neighbour's foot
        let az = |v: Vec3| v.z.atan2(v.x);
        let angdiff = |a: f32, b: f32| {
            let d = (a - b).rem_euclid(std::f32::consts::TAU);
            d.min(std::f32::consts::TAU - d)
        };
        let tgt_az: Vec<Option<f32>> = targets.iter().map(|t| t.map(az)).collect();
        let mut order: Vec<usize> = (0..self.legs.len()).collect();
        order.sort_by(|&a, &b| {
            az(self.legs[a].hip).partial_cmp(&az(self.legs[b].hip)).unwrap()
        });
        let mut used = [false; 8];
        for li in order {
            let leg = &self.legs[li];
            let la = az(leg.hip);
            let mut best: Option<usize> = None;
            let mut bestd = f32::MAX;
            for (ti, ta) in tgt_az.iter().enumerate() {
                if used[ti] {
                    continue;
                }
                if let Some(ta) = ta {
                    let d = angdiff(la, *ta);
                    if d < bestd {
                        bestd = d;
                        best = Some(ti);
                    }
                }
            }
            let Some(ti) = best else { continue };
            used[ti] = true;
            let target = targets[ti].unwrap();
            let (up_w, lo_w) = solve_leg_ik(leg, rest, target);
            let root_w = rest[leg.root];
            locals[leg.upper] = Some(root_w.inverse() * up_w);
            locals[leg.lower] = Some(up_w.inverse() * lo_w);
        }
        locals
    }
}

// 2-bone IK: rotate the rest upper/lower subtrees so the foot reaches `target`
// (model space), aiming each about its joint so bone lengths + mesh offsets hold
fn solve_leg_ik(leg: &SpiderLegRig, rest: &[Mat4], target: Vec3) -> (Mat4, Mat4) {
    let h = leg.hip;
    let dir = target - h;
    let dist = dir
        .length()
        .clamp((leg.l1 - leg.l2).abs() + 1e-3, leg.l1 + leg.l2 - 1e-3);
    let dir_n = dir.normalize_or_zero();
    // law of cosines: knee sits `a` along hip->target, `hgt` off the line
    let a = (dist * dist + leg.l1 * leg.l1 - leg.l2 * leg.l2) / (2.0 * dist);
    let hgt = (leg.l1 * leg.l1 - a * a).max(0.0).sqrt();
    let mut pole = leg.pole - dir_n * leg.pole.dot(dir_n);
    if pole.length() < 1e-4 {
        pole = (leg.knee - leg.hip) - dir_n * (leg.knee - leg.hip).dot(dir_n);
    }
    let pole = pole.normalize_or_zero();
    let knee_new = h + dir_n * a + pole * hgt;
    // upper: aim the rest subtree about the hip at knee_new
    let r_up = Quat::from_rotation_arc(
        (leg.knee - h).normalize_or_zero(),
        (knee_new - h).normalize_or_zero(),
    );
    let up_w = Mat4::from_translation(h)
        * Mat4::from_quat(r_up)
        * Mat4::from_translation(-h)
        * rest[leg.upper];
    // lower: aim the (now hip-rotated) shin at the target about knee_new
    let lo_moved = Mat4::from_translation(h)
        * Mat4::from_quat(r_up)
        * Mat4::from_translation(-h)
        * rest[leg.lower];
    let shin_rest = r_up * (leg.foot - leg.knee);
    let r_lo = Quat::from_rotation_arc(
        shin_rest.normalize_or_zero(),
        (target - knee_new).normalize_or_zero(),
    );
    let lo_w = Mat4::from_translation(knee_new)
        * Mat4::from_quat(r_lo)
        * Mat4::from_translation(-knee_new)
        * lo_moved;
    (up_w, lo_w)
}

// optional chain loop path next to the glb (see TrackPath)
fn load_track_path(glb_path: &Path) -> Option<TrackPath> {
    let p = glb_path.parent()?.join("track_path.json");
    let text = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let vec3 = |a: &serde_json::Value| -> Option<Vec3> {
        let a = a.as_array()?;
        Some(Vec3::new(
            a.first()?.as_f64()? as f32,
            a.get(1)?.as_f64()? as f32,
            a.get(2)?.as_f64()? as f32,
        ))
    };
    let points: Vec<Vec3> = v.get("points")?.as_array()?.iter().filter_map(vec3).collect();
    if !(3..=crate::model_renderer::MAX_TRACK_PTS).contains(&points.len()) {
        log::warn!("[model] track_path.json: bad point count {}", points.len());
        return None;
    }
    let lateral = v.get("lateral").and_then(vec3).unwrap_or(Vec3::Y).normalize();
    let mut cumlen = Vec::with_capacity(points.len());
    let mut total = 0.0f32;
    for i in 0..points.len() {
        cumlen.push(total);
        total += (points[(i + 1) % points.len()] - points[i]).length();
    }
    log::info!(
        "[model] track path: {} points, loop length {total:.2} ({})",
        points.len(),
        p.display()
    );
    Some(TrackPath { points, cumlen, total, lateral })
}

fn anim_duration(a: &gltf::Animation, buffers: &[gltf::buffer::Data]) -> f32 {
    let mut d = 0.0f32;
    for ch in a.channels() {
        let reader = ch.reader(|b| buffers.get(b.index()).map(|x| &x.0[..]));
        if let Some(t) = reader.read_inputs() {
            d = d.max(t.last().unwrap_or(0.0));
        }
    }
    d
}

// textures above this edge get box-downsampled: vram + ram + upload time
const MAX_TEX: u32 = 1024;

fn to_rgba(img: &gltf::image::Data) -> Option<TexData> {
    use gltf::image::Format;
    let (w, h) = (img.width, img.height);
    let rgba = match img.format {
        Format::R8G8B8A8 => img.pixels.clone(),
        Format::R8G8B8 => {
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for c in img.pixels.chunks_exact(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
            out
        }
        Format::R8 => {
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for &c in &img.pixels {
                out.extend_from_slice(&[c, c, c, 255]);
            }
            out
        }
        other => {
            log::warn!("[model] unsupported texture format {other:?} — using flat color");
            return None;
        }
    };
    let mut tex = TexData { width: w, height: h, rgba };
    while tex.width > MAX_TEX || tex.height > MAX_TEX {
        tex = halve(&tex);
    }
    Some(tex)
}

// 2x box downsample
fn halve(t: &TexData) -> TexData {
    let (w, h) = ((t.width / 2).max(1), (t.height / 2).max(1));
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = (x * 2, y * 2);
            for c in 0..4 {
                let mut sum = 0u32;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let px = (sx + dx).min(t.width - 1);
                    let py = (sy + dy).min(t.height - 1);
                    sum += t.rgba[((py * t.width + px) * 4 + c) as usize] as u32;
                }
                rgba.push((sum / 4) as u8);
            }
        }
    }
    TexData { width: w, height: h, rgba }
}

#[cfg(test)]
mod tests {
    // the 5 per-state player glbs: each must carry a skin, joints under the
    // shader bone cap, and its own animation clip. prints the numbers the
    // renderer will use (extent -> PLAYER_SIZE fit, duration -> clip pacing)
    #[test]
    fn load_player_models() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models");
        for key in crate::models::PLAYER_KEYS {
            let m = super::load(&root.join(key))
                .unwrap_or_else(|e| panic!("{key} failed to load: {e}"));
            assert!(!m.skins.is_empty(), "{key}: no skin");
            assert!(m.duration > 0.1, "{key}: no animation ({}s)", m.duration);
            let joints = m.skins.iter().map(|s| s.joints.len()).max().unwrap();
            assert!(joints <= crate::model_renderer::MAX_BONES, "{key}: {joints} joints");
            let ext = m.aabb_max - m.aabb_min;
            println!(
                "{key}: {} nodes, {} prims, {joints} joints, anim {:.2}s, \
                 extent x {:.2} y {:.2} z {:.2}",
                m.nodes.len(), m.prims.len(), m.duration, ext.x, ext.y, ext.z
            );
        }
    }

    // sample the skinned rigs exactly like the render path does, across the
    // whole clip: node worlds + skin matrices at many anim_t values. panics
    // here (bad indices, NaNs) would abort the game from the render hook
    #[test]
    fn sample_skinned_rigs() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models");
        let keys: Vec<String> = crate::models::PLAYER_KEYS
            .iter()
            .map(|k| k.to_string())
            .chain((1..=4).map(|t| format!("ENTITIES/ENEMIES/biters/biter_tier{t}.glb")))
            .collect();
        for key in keys {
            let path = root.join(&key);
            if !path.exists() {
                continue;
            }
            let m = super::load(&path).unwrap_or_else(|e| panic!("{key}: {e}"));
            for i in 0..=100 {
                let t = m.duration * i as f32 / 100.0;
                let worlds = m.node_worlds_posed(t, &super::Pose::default());
                for s in 0..m.skins.len() {
                    let mats = m.skin_matrices(s, &worlds);
                    for mat in &mats {
                        assert!(
                            mat.is_finite(),
                            "{key}: non-finite skin matrix at t={t}"
                        );
                    }
                }
            }
        }
    }

    // the rocket-silo export is 1708 separate meshes (2322 nodes) — merging
    // static prims by material must collapse it to ~one prim per material,
    // or a single placed silo costs thousands of draw calls per frame
    #[test]
    fn merge_static_prims() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models");
        let p = root.join("ENTITIES/STRUCTURES/rocket-silo/static.glb");
        if !p.exists() {
            return;
        }
        let m = super::load(&p).expect("rocket-silo must load");
        println!("rocket-silo: {} prims after merge", m.prims.len());
        assert!(m.prims.len() <= 16, "merge failed: {} prims", m.prims.len());
    }

    // the gate glb: its shapekey (morph target) drives the open/close motion
    // from the live openingProgress, so the loader must keep the morph deltas
    // and the weight track
    #[test]
    fn load_gate_model() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models");
        let p = root.join("ENTITIES/STRUCTURES/gate/static.glb");
        if !p.exists() {
            return;
        }
        let m = super::load(&p).expect("gate must load");
        assert!(m.weight_anim.is_some(), "gate: no morph weight track");
        let has_deltas = m.prims.iter().any(|pr| {
            let s = pr.src.lock().unwrap();
            s.as_ref().is_some_and(|s| {
                s.vertices
                    .chunks_exact(super::VERTEX_FLOATS)
                    .any(|v| v[8] != 0.0 || v[9] != 0.0 || v[10] != 0.0)
            })
        });
        assert!(has_deltas, "gate: no morph position deltas survived the merge");
        println!("gate: {} prims, anim {:.2}s", m.prims.len(), m.duration);
    }

    // the car glb (tools/export_car.py): the four wheel nodes must be
    // recognized as spinning wheels with a sane radius, or they'd merge into
    // the static hull and never roll
    #[test]
    fn load_car_model() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models");
        let p = root.join("ENTITIES/VEHICLES/car/static.glb");
        if !p.exists() {
            return;
        }
        let m = super::load(&p).expect("car must load");
        let wheels: Vec<_> = m.nodes.iter().filter(|n| n.is_wheel).collect();
        assert_eq!(wheels.len(), 4, "car: expected 4 wheel nodes");
        for w in &wheels {
            assert!(
                (0.3..1.0).contains(&w.wheel_radius),
                "car: odd wheel radius {}",
                w.wheel_radius
            );
        }
        assert!(m.has_pose_nodes, "car: wheels must flag pose nodes");
        let ext = m.aabb_max - m.aabb_min;
        println!("car: {} prims, extent x {:.2} y {:.2} z {:.2}", m.prims.len(), ext.x, ext.y, ext.z);
    }

    // foliage glbs: every prim must come out of the loader WITH its base
    // color texture (untextured = white blobs), and the tree cutout masks
    // baked by tools/patch_tree_alpha.py must survive as alpha variation
    #[test]
    fn load_tree_models() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models");
        let keys: Vec<String> = (1..=5)
            .map(|t| format!("ENTITIES/FOLIAGE/tree{t}/static.glb"))
            .chain(["branch1", "bush1", "rock1"].iter().map(|c| {
                format!("BLUEPRINTS/ground-system/ground-clutter/assets/{c}/static.glb")
            }))
            .collect();
        let mut masked = 0usize;
        for key in keys {
            let p = root.join(&key);
            if !p.exists() {
                continue;
            }
            let m = super::load(&p).unwrap_or_else(|e| panic!("{key}: {e}"));
            for (i, pr) in m.prims.iter().enumerate() {
                let s = pr.src.lock().unwrap();
                let s = s.as_ref().unwrap();
                // any pixel below half alpha = the cutout mask is present
                let has_mask = s.tex.as_ref().is_some_and(|t| {
                    t.rgba.chunks_exact(4).any(|px| px[3] < 128)
                });
                masked += has_mask as usize;
                println!(
                    "{key} prim{i} mat='{}' tex={:?} mask={has_mask} base_color={:?}",
                    pr.mat_name,
                    s.tex.as_ref().map(|t| (t.width, t.height)),
                    pr.base_color,
                );
                assert!(
                    s.tex.is_some() || pr.base_color[..3] != [1.0, 1.0, 1.0],
                    "{key} prim{i} ('{}') lost its texture -> would render white",
                    pr.mat_name
                );
            }
        }
        assert!(masked >= 5, "expected the baked tree cutout masks, found {masked}");
    }

    // every glb in the models tree must load; catches broken files early
    // (e.g. after tools/patch_pbr.py rewrites them). run with
    // `cargo test -p factorio_3d_models load_all -- --nocapture --ignored`
    #[test]
    #[ignore = "slow: loads every glb in ../models"]
    fn load_all_models() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../models");
        let mut checked = 0usize;
        let mut with_maps = 0usize;
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "glb") {
                    let m = super::load(&p)
                        .unwrap_or_else(|err| panic!("{} failed to load: {err}", p.display()));
                    checked += 1;
                    if m.prims.iter().any(|pr| {
                        let s = pr.src.lock().unwrap();
                        s.as_ref().is_some_and(|s| s.normal_tex.is_some() || s.mr_tex.is_some())
                    }) {
                        with_maps += 1;
                    }
                }
            }
        }
        println!("{checked} glbs loaded, {with_maps} carry normal/mr maps");
        assert!(checked > 0, "no glbs found under {}", root.display());
    }
}

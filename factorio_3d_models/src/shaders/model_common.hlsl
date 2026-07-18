cbuffer ModelCB : register(b0) {
    float4x4 mvp;
    float4x4 model;
    float4 baseColor;
    // x = has texture, y = morph weight, z = pass (0 lit,1 shadow,2 wire), w skinned
    float4 opts;
    // xyz = direction the sunlight travels
    float4 light;
    // xy = uv scroll on upward faces (belt treads), z = global alpha
    float4 misc;
    // xy = plane-space clip direction, zw = clip origin (composed junctions)
    float4 clipv;
    float4 mat;      // x metallic, y roughness, z emissive strength, w night
    float4 emissive; // xyz emissive color, w = emissive texture bound
    float4 cam;      // xyz camera pos, w exposure
    float4 sky;      // xyz sky ambient, w ambient strength
    float4 ground;   // xyz ground ambient, w rim strength
    float4 sun;      // xyz sun color, w sun strength
    // x night ambient floor, y specular scale, z env reflection, w flicker amount
    float4 params;
    // x = normal map bound (t2), y = metallic-roughness map bound (t3),
    // z = mr map is ORM-packed (r channel = baked ambient occlusion),
    // w = seconds running (emissive flicker phase)
    float4 pbr;
    // x = world-space height above which pixels are discarded (0 = off) —
    // hides the local player's head in first person. y > 0.5 = instanced
    // draw (world matrices come from `insts`, mvp holds viewProj only),
    // z = the draw's base slot in `insts`
    float4 clipy;
    // lamp point lights: xyz plane-space position, w falloff radius
    float4 lamps[16];
    // x = active lamp count, y = lamp strength
    float4 lampMeta;
};
cbuffer BonesCB : register(b1) {
    float4x4 bones[96];
};
cbuffer TrackCB : register(b2) {
    float4 tpts[64]; // xyz path point (node-local), w cumulative arc length
    float4 tmeta;    // x = count, y = total length, z/w = scroll per side
    float4 tlat;     // xyz = loop plane normal (toward the other chain band)
};

// per-instance world matrices for instanced draws (belt fields, belt items —
// thousands of copies of the same static model in one DrawIndexedInstanced)
StructuredBuffer<float4x4> insts : register(t4);

struct VS_IN {
    float3 pos  : POSITION0;
    float3 n    : NORMAL0;
    float2 uv   : TEXCOORD0;
    float3 dpos : POSITION1;
    float3 dn   : NORMAL1;
    float4 j    : BLENDINDICES0;
    float4 w    : BLENDWEIGHT0;
    uint   iid  : SV_InstanceID;
};
struct VS_OUT {
    float4 pos  : SV_POSITION;
    float3 n    : NORMAL;
    float2 uv   : TEXCOORD0;
    // world (plane-space) position: below-ground + junction-half clipping
    float3 wpos : TEXCOORD1;
};

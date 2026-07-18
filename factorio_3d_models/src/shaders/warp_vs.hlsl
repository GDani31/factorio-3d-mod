// maps the whole fbo (uv 0..1) onto a ground plane extended by planeScale,
// centered on the camera point (fbo center)
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

VS_OUT main(uint id : SV_VertexID) {
    float2 uv = float2(id & 1, id >> 1);
    float3 worldPos = float3(
        (uv.x - 0.5) * 2.0 * aspect * planeScale,
        0.0,
        (0.5 - uv.y) * 2.0 * planeScale
    );
    VS_OUT o;
    o.pos = mul(viewProj, float4(worldPos, 1.0));
    o.uv = uv;
    return o;
}

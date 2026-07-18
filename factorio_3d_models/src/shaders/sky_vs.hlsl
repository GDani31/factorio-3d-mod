// fullscreen quad from SV_VertexID (triangle strip, 4 verts, no vertex buffer)
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

VS_OUT main(uint id : SV_VertexID) {
    VS_OUT o;
    float2 xy = float2(id & 1, id >> 1);
    o.uv = xy;
    o.pos = float4(xy.x * 2.0 - 1.0, 1.0 - xy.y * 2.0, 0.0, 1.0);
    return o;
}

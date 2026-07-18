// sharp-bilinear sampling (plain bilinear turns the magnified near field
// into mush)
Texture2D sceneTex : register(t0);
SamplerState samp  : register(s0);

float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float sharpen = max(planeScale * 4.0, 6.0);
    float2 ts = float2(texW, texH);
    float2 p = uv * ts;
    float2 tf = floor(p);
    float2 cd = (p - tf) - 0.5;
    float2 rr = 0.5 - 0.5 / sharpen;
    float2 f = (cd - clamp(cd, -rr, rr)) * sharpen + 0.5;
    return sceneTex.Sample(samp, (tf + f) / ts);
}

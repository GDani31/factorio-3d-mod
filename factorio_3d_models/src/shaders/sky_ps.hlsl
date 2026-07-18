// procedural nauvis sky: pale haze at the horizon into blue zenith by day,
// a warm band while the sun sets, near-black at night. sky.x = night 0..1
float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float night = saturate(sky.x);
    // 4n(1-n) peaks mid-transition: the golden-hour window
    float dusk = 4.0 * night * (1.0 - night);
    float t = saturate(1.0 - uv.y); // 1 = top of screen (zenith)

    float3 zen = lerp(float3(0.30, 0.53, 0.85), float3(0.004, 0.006, 0.016), night);
    float3 hor = lerp(float3(0.76, 0.85, 0.95), float3(0.012, 0.018, 0.040), night);
    // sunset warmth pools at the horizon, gone by full night
    hor = lerp(hor, float3(0.98, 0.55, 0.28), dusk * 0.5);

    float3 c = lerp(hor, zen, pow(t, 1.5));
    return float4(c, 1.0);
}

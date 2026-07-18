Texture2D tex       : register(t0);
Texture2D emisTex   : register(t1);
Texture2D normalTex : register(t2);
Texture2D mrTex     : register(t3); // g roughness, b metallic (r ao when ORM)
SamplerState samp   : register(s0);

// jpg/png albedo is stored gamma-encoded; light in linear, display in gamma
float3 toLinear(float3 c) { return pow(max(c, 0.0), 2.2); }
float3 toSRGB(float3 c)   { return pow(max(c, 0.0), 1.0 / 2.2); }

// ACES filmic tonemap (Narkowicz fit) — rolls off highlights so metal
// glints and emissives bloom-clip gracefully instead of turning flat white
float3 aces(float3 x) {
    return saturate((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14));
}

float4 main(float4 pos : SV_POSITION, float3 n : NORMAL, float2 uv : TEXCOORD0,
            float3 wpos : TEXCOORD1) : SV_TARGET {
    // hide geometry authored below ground level (rocket silo shaft etc.)
    clip(wpos.y + 1e-4);
    // first person: the local player's head sits inside the camera — hide
    // everything above the neck (wpos is unflattened, so this also removes
    // the head from the ground-shadow pass)
    if (clipy.x > 0.0)
        clip(clipy.x - wpos.y);
    // composed junction: keep only the half toward the clip direction
    if (abs(clipv.x) + abs(clipv.y) > 0.001)
        clip(dot(wpos.xz - clipv.zw, clipv.xy));

    float3 N = normalize(n);

    // wire pass: bright colored cable, lightly shaded so it reads as a tube,
    // tonemapped so it sits in the same look as the models
    if (opts.z > 1.5) {
        float3 L = normalize(-light.xyz);
        float d = saturate(dot(N, L)) * 0.4 + 0.6;
        float3 c = toSRGB(aces(toLinear(baseColor.rgb) * d * cam.w));
        return float4(c, misc.z);
    }
    // shadow pass: flat translucent black (geometry already flattened)
    if (opts.z > 0.5)
        return baseColor;

    float4 albedo4 = baseColor;
    if (opts.x > 0.5) {
        albedo4 *= tex.Sample(samp, uv);
        // alpha cutout: foliage cards carry their opacity mask in the
        // texture alpha (baked by tools/patch_tree_alpha.py). jpg-sourced
        // textures are a=1 everywhere, so only masked models are affected
        if (albedo4.a < 0.5)
            discard;
    }
    float3 albedo = toLinear(albedo4.rgb);

    // tangent-space normal map via the screen-space cotangent frame — no
    // vertex tangents needed, the frame is rebuilt from position/uv gradients
    if (pbr.x > 0.5) {
        float3 nm = normalTex.Sample(samp, uv).rgb * 2.0 - 1.0;
        float3 dp1 = ddx(wpos), dp2 = ddy(wpos);
        float2 du1 = ddx(uv),   du2 = ddy(uv);
        float3 dp2perp = cross(dp2, N);
        float3 dp1perp = cross(N, dp1);
        float3 T = dp2perp * du1.x + dp1perp * du2.x;
        float3 B = dp2perp * du1.y + dp1perp * du2.y;
        float invmax = rsqrt(max(max(dot(T, T), dot(B, B)), 1e-12));
        N = normalize(mul(nm, float3x3(T * invmax, B * invmax, N)));
    }

    float metallic = saturate(mat.x);
    float rough    = mat.y;
    float ao       = 1.0;
    // metallic-roughness map: gltf packs roughness in g, metallic in b; the
    // cpu-side factors in mat.xy multiply on top (gltf spec). ORM exports
    // carry baked occlusion in r — darkens ambient/reflection, not the sun
    if (pbr.y > 0.5) {
        float3 orm = mrTex.Sample(samp, uv).rgb;
        rough    *= orm.g;
        metallic  = saturate(metallic * orm.b);
        if (pbr.z > 0.5)
            ao = orm.r;
    }
    rough = clamp(rough, 0.05, 1.0);
    float night = saturate(mat.w);
    float day   = 1.0 - night;

    float3 L = normalize(-light.xyz);        // toward the sun
    float3 V = normalize(cam.xyz - wpos);    // toward the camera
    float3 H = normalize(L + V);

    // hemispheric ambient: sky from above, warm bounce from below. dims to a
    // moonlit floor at night instead of going black. baked ao darkens crevices
    float hemi = N.y * 0.5 + 0.5;
    float3 ambCol = lerp(ground.xyz, sky.xyz, hemi) * sky.w * ao;
    ambCol *= lerp(params.x, 1.0, day);

    // sun: soft wrapped diffuse so the terminator isn't a hard line
    float wrap = saturate((dot(N, L) + 0.35) / 1.35);
    float3 sunCol = sun.xyz * sun.w * day;

    // dielectric F0 0.04, metals tint their reflection with the albedo
    float3 F0 = lerp(float3(0.04, 0.04, 0.04), albedo, metallic);
    float ndv = saturate(dot(N, V));
    // roughness-aware schlick fresnel — grazing angles reflect more, and a
    // smooth surface reflects across its whole face while a rough one only
    // at the very edge (F90 pulled down by roughness)
    float3 Fr = F0 + (max(1.0 - rough, F0) - F0) * pow(1.0 - ndv, 5.0);

    // sun specular: GGX microfacets (trowbridge-reitz NDF + smith-schlick
    // visibility) — the long highlight tails blinn-phong can't do; rough
    // surfaces spread the glint wide, smooth metal gets a tight hot core
    float ndh = saturate(dot(N, H));
    float ndl = saturate(dot(N, L));
    float a  = rough * rough;
    float a2 = a * a;
    float dd = ndh * ndh * (a2 - 1.0) + 1.0;
    float D  = a2 / max(3.14159 * dd * dd, 1e-6);
    float k  = a * 0.5;
    float vis = 0.25 / max((ndv * (1.0 - k) + k) * (ndl * (1.0 - k) + k), 1e-4);
    float3 specular = Fr * (D * vis) * ndl * sunCol * params.y;

    // environment reflection (fake IBL): no cubemap, so reflect the view ray
    // and sample the same sky/ground hemisphere it would otherwise see —
    // upward reflections catch the sky, downward the warm ground bounce.
    // smooth metal mirrors it crisply, rough matte casings barely do
    float3 R = reflect(-V, N);
    float3 envCol = lerp(ground.xyz, sky.xyz, R.y * 0.5 + 0.5);
    float3 envRefl = envCol * Fr * params.z * lerp(0.2, 1.0, 1.0 - rough)
                     * lerp(params.x, 1.0, day) * ao;

    // diffuse minus what the fresnel reflected away (energy conservation, so
    // the reflection doesn't just pile on brightness) and near-zero for metals
    float3 kd = (1.0 - Fr) * lerp(1.0, 0.2, metallic);
    float3 diffuse = albedo * (ambCol + sunCol * wrap) * kd;

    // fresnel rim toward the sky — the subtle edge pop UE models have
    float rim = pow(1.0 - ndv, 3.0) * ground.w * ao;
    float3 rimCol = sky.xyz * rim * lerp(params.x, 1.0, day);

    // lamp point lights: warm pools around lamps, fading in as night falls
    float3 lampAcc = float3(0.0, 0.0, 0.0);
    int nl = (int)lampMeta.x;
    for (int li = 0; li < nl; li++) {
        float3 dv = lamps[li].xyz - wpos;
        float dist = length(dv);
        float att = saturate(1.0 - dist / max(lamps[li].w, 1e-3));
        att *= att;
        // half-lambert: faces toward the lamp lit most, back faces still rim-lit
        float nd = saturate(dot(N, dv / max(dist, 1e-3))) * 0.6 + 0.4;
        lampAcc += att * nd;
    }
    float3 lampCol = albedo * kd * float3(1.0, 0.82, 0.55)
                     * lampAcc * lampMeta.y * night;

    // emissive glows day and night, stronger at night (lamps come on)
    float3 emis = emissive.xyz;
    if (emissive.w > 0.5)
        emis *= toLinear(emisTex.Sample(samp, uv).rgb);
    emis *= mat.z * (1.0 + night * 2.0);
    // organic shimmer at night (two incommensurate sines ≈ fire flicker) —
    // ported from the FUE5 light-function material (Time->Sine->Multiply)
    emis *= 1.0 + params.w * night
                * (0.55 * sin(pbr.w * 9.7) + 0.45 * sin(pbr.w * 23.3));

    float3 color = diffuse + specular + envRefl + rimCol + lampCol + emis;
    color = aces(color * cam.w);   // exposure + filmic tonemap
    color = toSRGB(color);
    // misc.z < 1 while ALT is held: see-through so the alt overlay shows
    return float4(color, misc.z);
}

VS_OUT main(VS_IN i) {
    VS_OUT o;
    float3 p = i.pos + opts.y * i.dpos;
    float3 n = i.n + opts.y * i.dn;
    // chain conveyor (misc.w = 1): slide this vertex ALONG the closed loop
    // path by tmeta.z arc length. links flow around the sprockets instead of
    // the whole mesh translating (which wobbled at the ends)
    if (misc.w > 0.5) {
        int cnt = (int)tmeta.x;
        float L = tmeta.y;
        // closest path segment, distances measured in the loop plane
        float best = 1e30; int bi = 0; float3 bproj = p;
        for (int s = 0; s < cnt; s++) {
            float3 a = tpts[s].xyz;
            float3 b = tpts[s + 1 < cnt ? s + 1 : 0].xyz;
            float3 ab = b - a;
            float t = saturate(dot(p - a, ab) / max(dot(ab, ab), 1e-6));
            float3 pr = a + ab * t;
            float3 d = (p - pr) - tlat.xyz * dot(p - pr, tlat.xyz);
            float dd = dot(d, d);
            if (dd < best) { best = dd; bi = s; bproj = pr; }
        }
        float3 a1 = tpts[bi].xyz;
        float3 t1 = normalize(tpts[bi + 1 < cnt ? bi + 1 : 0].xyz - a1);
        float3 m1 = normalize(cross(tlat.xyz, t1));
        float s1 = tpts[bi].w + length(bproj - a1);
        // the two chain bands sit on opposite lateral sides of the path
        // plane and scroll independently (differential turning)
        float toff = dot(p - bproj, tlat.xyz) > 0.0 ? tmeta.z : tmeta.w;
        // destination arc position (wrapped into [0, L))
        float s2 = fmod(fmod(s1 + toff, L) + L, L);
        int bj = cnt - 1;
        for (int k = 0; k < cnt; k++) {
            float e = k + 1 < cnt ? tpts[k + 1].w : L;
            if (s2 < e) { bj = k; break; }
        }
        float3 a2 = tpts[bj].xyz;
        float3 t2 = normalize(tpts[bj + 1 < cnt ? bj + 1 : 0].xyz - a2);
        float3 m2 = normalize(cross(tlat.xyz, t2));
        float3 proj2 = a2 + t2 * (s2 - tpts[bj].w);
        // re-express the vertex offset in the destination frame: links
        // rotate rigidly as they round the sprockets
        float3 rel = p - bproj;
        p = proj2 + t2 * dot(rel, t1) + m2 * dot(rel, m1) + tlat.xyz * dot(rel, tlat.xyz);
        n = t2 * dot(n, t1) + m2 * dot(n, m1) + tlat.xyz * dot(n, tlat.xyz);
    }
    if (opts.w > 0.5) { // skinned: bones replace the node transform
        float3 sp = 0, sn = 0;
        [unroll] for (int k = 0; k < 4; k++) {
            float w = i.w[k];
            if (w > 0.0) {
                float4x4 b = bones[(int)i.j[k]];
                sp += w * mul(b, float4(p, 1.0)).xyz;
                sn += w * mul((float3x3)b, n);
            }
        }
        p = sp;
        n = sn;
    }
    // instanced: the per-copy world comes from the structured buffer and mvp
    // holds only viewProj (shadow pass: viewProj * flatten)
    if (clipy.y > 0.5) {
        float4x4 W = insts[(uint)clipy.z + i.iid];
        float4 wp = mul(W, float4(p, 1.0));
        o.pos = mul(mvp, wp);
        o.n = mul((float3x3)W, n);
        o.uv = i.uv;
        if (normalize(o.n).y > 0.5)
            o.uv += misc.xy;
        o.wpos = wp.xyz;
        return o;
    }
    o.pos = mul(mvp, float4(p, 1.0));
    o.n = mul((float3x3)model, n);
    o.uv = i.uv;
    // belt treads: scroll the texture on faces pointing up IN WORLD SPACE
    // (the c4d node transforms rotate the local axes)
    if (normalize(o.n).y > 0.5)
        o.uv += misc.xy;
    o.wpos = mul(model, float4(p, 1.0)).xyz;
    return o;
}

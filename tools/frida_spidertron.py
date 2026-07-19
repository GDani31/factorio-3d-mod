import frida, sys

# hooks SpiderVehicle::draw and dumps what the 3d mod reads to drive the rig:
# body position + torso orientation + height, and each leg's world foot.
# throttled to one spider per second. offsets are factorio 2.0.77.
#
#   SpiderVehicle::draw   rva 0x51ECA0   (rcx = SpiderVehicle*, this)
#   body pos              [this+0x50]    MapPosition, 2x i32 in 1/256 tile
#   torso orientation     [this+0x348]   f32 RealOrientation 0..1 (the "head")
#   body height           [this+0x320]   f32 (lift above ground)
#   leg vector (inside the embedded SpiderEngine at this+0x2c0):
#     begin ptr           [this+0x2d0]   (engine+0x10)
#     end   ptr           [this+0x2d8]   (engine+0x18), stride 0x20 per entry
#     SpiderLeg*          [entry+0x08]
#   foot: each SpiderLeg is an entity positioned AT its foot, so its
#     foot pos            [leg+0x50]     MapPosition (2x i32/256) = world foot
#
# note: leg+0xe8 points to a per-leg walk-state, but its fields turned out to
# be small constants, NOT the foot — the foot is just the leg's own position.

DRAW_RVA = 0x51ECA0

session = frida.attach("factorio.exe")
script = session.create_script(r"""
const base = Process.getModuleByName('factorio.exe').base;

function mappos(p) {            // 2x i32, 1/256 fixed point -> tiles
    return [p.readS32() / 256.0, p.add(4).readS32() / 256.0];
}

let last = 0;
Interceptor.attach(base.add(%d), {
    onEnter(args) {
        const now = Date.now();
        if (now - last < 1000) return;     // one spider / second
        last = now;
        const self = args[0];              // SpiderVehicle*
        try {
            const pos   = mappos(self.add(0x50));
            const orient= self.add(0x348).readFloat();
            const height= self.add(0x320).readFloat();

            let out = {pos, orient, height, feet: []};

            const begin = self.add(0x2d0).readPointer();
            const end   = self.add(0x2d8).readPointer();
            for (let e = begin; e.compare(end) < 0; e = e.add(0x20)) {
                const leg = e.add(0x08).readPointer();
                if (leg.isNull()) continue;
                out.feet.push(mappos(leg.add(0x50)));   // the leg IS its foot
            }
            send(out);
        } catch (err) { send({err: '' + err}); }
    }
});
""" % DRAW_RVA)

def fmt(p):
    return "(%.3f, %.3f)" % (p[0], p[1]) if p else "None"

def on_message(msg, data):
    if msg["type"] != "send":
        print(msg); return
    p = msg["payload"]
    if "err" in p:
        print("err:", p["err"]); return
    print("=" * 64)
    print("body pos  %s   orient %.4f (%.1f deg)   height %.3f"
          % (fmt(p["pos"]), p["orient"], p["orient"] * 360.0, p["height"]))
    for i, foot in enumerate(p["feet"]):
        # dx,dy = foot relative to the body (what the leg reaches for)
        dx, dy = foot[0] - p["pos"][0], foot[1] - p["pos"][1]
        print("  leg %d  foot %s   rel (%+.2f, %+.2f)" % (i, fmt(foot), dx, dy))

script.on("message", on_message)
script.load()
input("hooked SpiderVehicle::draw. drive a spidertron around, then Enter to quit.\n")

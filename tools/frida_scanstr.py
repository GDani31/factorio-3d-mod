import frida, sys

rva = int(sys.argv[1], 16)
argn = int(sys.argv[2]) if len(sys.argv) > 2 else 0
size = int(sys.argv[3], 0) if len(sys.argv) > 3 else 0x600

session = frida.attach("factorio.exe")
script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    let done = false;
    function ok(p, n) { try { p.readByteArray(n); return true; } catch (e) { return false; } }
    Interceptor.attach(base.add(%d), {
        onEnter(args) {
            if (done) return; done = true;
            const obj = args[%d];
            let out = [];
            for (let off = 0; off < %d; off += 8) {
                const sp = obj.add(off);
                if (!ok(sp, 32)) continue;
                const sz = sp.add(0x10).readU64().toNumber();
                const cap = sp.add(0x18).readU64().toNumber();
                if (sz < 2 || sz > 64 || cap < sz) continue;
                const data = cap == 15 ? sp : sp.readPointer();
                if (!ok(data, sz)) continue;
                const arr = new Uint8Array(data.readByteArray(sz));
                if (arr.every(b => b >= 0x20 && b < 0x7f))
                    out.push('+0x' + off.toString(16) + ' = "' + String.fromCharCode.apply(null, arr) + '"');
            }
            send(out.length ? out.join('\n') : 'no strings found');
        }
    });
""" % (rva, argn, size))

script.on('message', lambda m, d: print(m['payload']))
script.load()
input('Enter to quit.\n')

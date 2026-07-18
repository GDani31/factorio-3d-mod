import frida, sys

rva = int(sys.argv[1], 16)
argn = int(sys.argv[2]) if len(sys.argv) > 2 else 0
n = int(sys.argv[3]) if len(sys.argv) > 3 else 8

session = frida.attach("factorio.exe")
script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    let done = false;
    Interceptor.attach(base.add(%d), {
        onEnter(args) {
            if (done) return; done = true;
            const vt = args[%d].readPointer();
            let s = 'vtable @ +0x' + vt.sub(base).toString(16);
            for (let i = 0; i < %d; i++)
                s += '\n  [' + i + '] +0x' + vt.add(i * 8).readPointer().sub(base).toString(16);
            send(s);
        }
    });
""" % (rva, argn, n))

script.on('message', lambda m, d: print(m['payload']))
script.load()
input('Enter to quit.\n')

import frida, sys

rva = int(sys.argv[1], 16)
argc = int(sys.argv[2]) if len(sys.argv) > 2 else 4

session = frida.attach("factorio.exe")
script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    const rva = %d, argc = %d;
    Interceptor.attach(base.add(rva), {
        onEnter(args) {
            let s = 'call +0x' + rva.toString(16);
            for (let i = 0; i < argc; i++) s += '\n  arg' + i + ' = ' + args[i];
            send(s);
        }
    });
""" % (rva, argc))

script.on('message', lambda m, d: print(m['payload']))
script.load()
input('hooked +0x%x. Enter to quit.\n' % rva)

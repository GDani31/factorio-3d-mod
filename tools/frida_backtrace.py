import frida, sys

rva = int(sys.argv[1], 16)

session = frida.attach("factorio.exe")
script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    let done = false;
    Interceptor.attach(base.add(%d), {
        onEnter(args) {
            if (done) return; done = true;
            const bt = Thread.backtrace(this.context, Backtracer.ACCURATE)
                .map(a => '+0x' + a.sub(base).toString(16)).join('\n  ');
            send('caller chain (rva):\n  ' + bt);
        }
    });
""" % rva)

script.on('message', lambda m, d: print(m['payload']))
script.load()
input('Enter to quit.\n')

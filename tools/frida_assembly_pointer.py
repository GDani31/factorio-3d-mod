import frida

session = frida.attach("factorio.exe")

script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    Interceptor.attach(base.add(0x322BC0), {                 // AssemblingMachine::draw
        onEnter(args) {
            const self = args[0];                            // the assembler
            const x = self.add(0x50).readS32() / 256;        // position field +0x50
            const y = self.add(0x54).readS32() / 256;        // +0x54
            send('this=' + self + '  pos=(' + x + ', ' + y + ')');
        }
    });
""")

script.on('message', lambda m, d: print(m['payload']))
script.load()
input('hooked. Enter to quit.\n')

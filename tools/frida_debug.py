import frida

session = frida.attach("factorio.exe")

script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    Interceptor.attach(base.add(0x322BC0), {                 // AssemblingMachine::draw
        onEnter(args) {
            send('assembler this = ' + args[0]);             // args[0] = the assembler
        }
    });
""")

script.on('message', lambda m, d: print(m['payload']))
script.load()
input('hooked. look at an assembler + scroll the map. Enter to quit.\n')

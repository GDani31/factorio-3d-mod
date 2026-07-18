import frida, struct

session = frida.attach("factorio.exe")

script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    let done = false;
    Interceptor.attach(base.add(0x322BC0), {          // AssemblingMachine::draw
        onEnter(args) {
            if (done) return;                          // dump ONE object, then stop
            done = true;
            send({this: args[0].toString()}, args[0].readByteArray(0x300));
        }
    });
""")

def on_message(msg, data):
    if msg['type'] != 'send' or data is None:
        print(msg); return
    print(f"\n=== assembler object at {msg['payload']['this']} ===")
    print(f"{'offset':>7} {'int32':>12} {'int32/256':>11} {'float':>13}   u64 hex (pointer?)")
    for off in range(0, len(data) - 8, 4):
        i32 = struct.unpack_from('<i', data, off)[0]
        f32 = struct.unpack_from('<f', data, off)[0]
        u64 = struct.unpack_from('<Q', data, off)[0]
        print(f"+0x{off:03x} {i32:12d} {i32/256:11.3f} {f32:13.4g}   0x{u64:016x}")

script.on('message', on_message)
script.load()
print("look at an assembler + scroll the map (dumps the first one, then stops)")
input("Enter to quit.\n")

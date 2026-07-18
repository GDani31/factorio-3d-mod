import frida, struct, sys

rva = int(sys.argv[1], 16)
argn = int(sys.argv[2]) if len(sys.argv) > 2 else 0
size = int(sys.argv[3], 0) if len(sys.argv) > 3 else 0x200

session = frida.attach("factorio.exe")
script = session.create_script(r"""
    const base = Process.getModuleByName('factorio.exe').base;
    let done = false;
    Interceptor.attach(base.add(%d), {
        onEnter(args) {
            if (done) return; done = true;
            send({p: args[%d].toString()}, args[%d].readByteArray(%d));
        }
    });
""" % (rva, argn, argn, size))

def on_message(msg, data):
    if msg['type'] != 'send' or data is None:
        print(msg); return
    print("object at " + msg['payload']['p'])
    print(f"{'offset':>7} {'int32':>12} {'int32/256':>11} {'float':>13}   u64 hex")
    for off in range(0, len(data) - 8, 4):
        i32 = struct.unpack_from('<i', data, off)[0]
        f32 = struct.unpack_from('<f', data, off)[0]
        u64 = struct.unpack_from('<Q', data, off)[0]
        print(f"+0x{off:03x} {i32:12d} {i32/256:11.3f} {f32:13.4g}   0x{u64:016x}")

script.on('message', on_message)
script.load()
input('Enter to quit.\n')

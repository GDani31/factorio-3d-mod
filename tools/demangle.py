import ctypes, sys

d = ctypes.windll.dbghelp
d.UnDecorateSymbolName.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint, ctypes.c_uint]
d.UnDecorateSymbolName.restype = ctypes.c_uint

def un(name, flags=0):
    buf = ctypes.create_string_buffer(4096)
    n = d.UnDecorateSymbolName(name.encode(), buf, 4096, flags)
    return buf.value.decode(errors="replace") if n else name

names = sys.argv[1:] or [l.strip() for l in sys.stdin if l.strip()]
for x in names:
    print(x)
    print("  " + un(x))

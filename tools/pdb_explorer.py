import ctypes as C, sys, os
from ctypes import wintypes

dbghelp = C.windll.dbghelp
kernel32 = C.windll.kernel32

SYMOPT_UNDNAME = 0x00000002
SYMOPT_PUBLICS_ONLY = 0x00004000
HANDLE = C.c_void_p
H = HANDLE(0x1000)
LOAD_BASE = 0x10000000
LOAD_SIZE = 0x08000000


class SYMBOL_INFO(C.Structure):
    _fields_ = [
        ("SizeOfStruct", wintypes.ULONG),
        ("TypeIndex", wintypes.ULONG),
        ("Reserved", C.c_ulonglong * 2),
        ("Index", wintypes.ULONG),
        ("Size", wintypes.ULONG),
        ("ModBase", C.c_ulonglong),
        ("Flags", wintypes.ULONG),
        ("Value", C.c_ulonglong),
        ("Address", C.c_ulonglong),
        ("Register", wintypes.ULONG),
        ("Scope", wintypes.ULONG),
        ("Tag", wintypes.ULONG),
        ("NameLen", wintypes.ULONG),
        ("MaxNameLen", wintypes.ULONG),
        ("Name", C.c_char * 1),
    ]


NAME_OFF = SYMBOL_INFO.Name.offset
CB = C.WINFUNCTYPE(wintypes.BOOL, C.POINTER(SYMBOL_INFO), wintypes.ULONG, C.c_void_p)

dbghelp.SymLoadModuleEx.argtypes = [HANDLE, HANDLE, C.c_char_p, C.c_char_p,
                                    C.c_ulonglong, wintypes.DWORD, C.c_void_p, wintypes.DWORD]
dbghelp.SymLoadModuleEx.restype = C.c_ulonglong
dbghelp.SymEnumSymbols.argtypes = [HANDLE, C.c_ulonglong, C.c_char_p, CB, C.c_void_p]
dbghelp.SymEnumSymbols.restype = wintypes.BOOL


def load(pdb):
    dbghelp.SymSetOptions((dbghelp.SymGetOptions() & ~SYMOPT_UNDNAME) | SYMOPT_PUBLICS_ONLY)
    if not dbghelp.SymInitialize(H, None, False):
        raise OSError(f"SymInitialize failed ({kernel32.GetLastError()})")
    for img in (pdb, os.path.join(os.path.dirname(pdb), "factorio.exe")):
        if not os.path.exists(img):
            continue
        base = dbghelp.SymLoadModuleEx(H, None, img.encode(), None, LOAD_BASE, LOAD_SIZE, None, 0)
        if base:
            return base
    raise OSError(f"SymLoadModuleEx failed ({kernel32.GetLastError()})")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    pdb = args[0]
    term = args[1] if len(args) > 1 else "render"
    mx = 100
    for a in sys.argv:
        if a.startswith("--max="):
            mx = int(a.split("=", 1)[1])
    base = load(pdb)
    found = []

    def collect(psym, size, ctx):
        info = psym.contents
        addr = C.cast(psym, C.c_void_p).value + NAME_OFF
        name = C.string_at(addr, info.NameLen).decode("ascii", "replace")
        found.append((name, info.Address - base))
        return True

    dbghelp.SymEnumSymbols(H, base, f"*{term}*".encode(), CB(collect), None)
    found = sorted(set(found))
    print(f"{'RVA':<12} Symbol")
    print(f"{'-'*11} {'-'*60}")
    for name, rva in found[:mx]:
        print(f"0x{rva & 0xFFFFFFFF:08X}  {name}")
    print(f"\n--- {len(found)} symbols matching \"{term}\" ---")
    if len(found) > mx:
        print(f"(showing first {mx}, pass --max=N for more)")
    dbghelp.SymCleanup(H)


if __name__ == "__main__":
    main()

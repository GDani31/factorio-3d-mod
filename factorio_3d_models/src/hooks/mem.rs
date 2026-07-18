// safe probing of game memory: nothing here dereferences a pointer without
// checking the page is actually readable first.

use crate::offsets;

// true if [ptr, ptr+len) is committed readable memory
pub fn readable(ptr: *const u8, len: usize) -> bool {
    use windows::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    if ptr.is_null() || len == 0 {
        return false;
    }
    let mut checked = 0usize;
    while checked < len {
        let p = unsafe { ptr.add(checked) };
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let r = unsafe {
            VirtualQuery(
                Some(p as *const core::ffi::c_void),
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if r == 0 || mbi.State != MEM_COMMIT {
            return false;
        }
        // 0xEE = every PAGE_* protection that allows reads
        const READABLE_PROT: u32 = 0xEE;
        let prot = mbi.Protect.0;
        if prot & (PAGE_NOACCESS.0 | PAGE_GUARD.0) != 0 || prot & READABLE_PROT == 0 {
            return false;
        }
        let region_end = mbi.BaseAddress as usize + mbi.RegionSize;
        checked = region_end.saturating_sub(ptr as usize);
    }
    true
}

pub fn read<T: Copy>(addr: usize) -> T {
    unsafe { std::ptr::read_unaligned(addr as *const T) }
}

// like read but None when the memory isn't readable
pub fn try_read<T: Copy>(addr: usize) -> Option<T> {
    readable(addr as *const u8, std::mem::size_of::<T>()).then(|| read(addr))
}

// scan an object for msvc std::string fields and return their contents.
// prototype names ("assembling-machine-1") live at a small offset.
pub fn scan_proto_strings(proto: *const u8) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for off in (0..0x600usize).step_by(8) {
        let sp = unsafe { proto.add(off) };
        if !readable(sp, 32) {
            continue;
        }
        let size = unsafe { std::ptr::read_unaligned(sp.add(0x10) as *const u64) } as usize;
        let cap = unsafe { std::ptr::read_unaligned(sp.add(0x18) as *const u64) } as usize;
        if !(3..=48).contains(&size) || cap < size || cap < 15 || cap >= 0x1000 {
            continue;
        }
        // sso strings (cap 15) live inline; longer ones behind a heap pointer
        let data_ptr = if cap == 15 {
            if size >= 16 {
                continue;
            }
            sp
        } else {
            let hp = unsafe { std::ptr::read_unaligned(sp as *const u64) } as *const u8;
            if !readable(hp, size + 1) {
                continue;
            }
            hp
        };
        let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size + 1) };
        if bytes[size] != 0 || !bytes[..size].iter().all(|&b| (0x20..0x7F).contains(&b)) {
            continue;
        }
        found.push((off, String::from_utf8_lossy(&bytes[..size]).into_owned()));
        if found.len() >= 8 {
            break;
        }
    }
    found
}

// true while the DrawQueue is in ghost mode (build-cursor preview / ghosts)
pub fn dq_is_ghost(dq: usize) -> bool {
    dq != 0 && read::<u8>(dq + offsets::DQ_GHOST_MODE) != 0
}

// camera-facing sprite rotation: hooks the functions that pick which
// rotation frame / direction variant gets drawn, and adds the camera yaw.
//
// DANGER ZONE. the three leaf pickers are tiny functions whose callers were
// compiled with whole-program optimization: they keep live values in
// volatile registers across the call because they KNOW the callee doesn't
// touch them. a normal detour (rust code clobbers all volatile registers)
// corrupts those callers and crashes. so the leaf hooks are naked asm shims
// that save every volatile register, call a small compute function for the
// substituted argument, restore everything, and call the original.

use crate::offsets;
use crate::settings::CHAR_ROT_SIGN;
use crate::symbols::SymbolMap;
use anyhow::Result;
use retour::static_detour;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::MapPos;

static_detour! {
    // SpriteNWay<4>::draw(DrawQueue&, MapPosition const&, Direction (byte), RenderLayer)
    static SpriteNWay4DrawHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, *const MapPos, u8, u8);
    // drawScaledRotated — the inserter-arm path (hook fn lives in sprites.rs)
    pub(super) static DrawScaledRotatedHook: unsafe extern "C" fn(*mut core::ffi::c_void, *const core::ffi::c_void, *const MapPos, *const f32, f64, f64, u32, u8, *const core::ffi::c_void, i8);
}

// the yaw as an orientation delta (fraction of a full turn), or None while
// the 3d view is off.
//
// PHASE-GATED: the hooked pickers are also called from entity UPDATE code.
// rotation only applies while a draw context is on this thread's stack, so
// no camera state can ever leak into the simulation
#[inline]
pub(super) fn frame_rot_delta() -> Option<f32> {
    let in_draw = super::IN_RENDER_PREPARE.with(|f| f.get())
        || super::ENTITY_DRAW_DEPTH.with(|d| d.get()) > 0
        || super::STATIC_DRAW_DEPTH.with(|d| d.get()) > 0
        || super::IN_CHARACTER_DRAW.with(|f| f.get());
    if !in_draw || !crate::capture::capture_enabled() {
        return None;
    }
    let (yaw, _p, _z) = crate::camera::get();
    if yaw.abs() <= 1.0 {
        return None;
    }
    Some((CHAR_ROT_SIGN as f32) * yaw / 360.0)
}

// camera quadrant for 4-way direction sprites (0..3 steps of 90 degrees)
#[inline]
pub(super) fn dir_rot_steps() -> u8 {
    match frame_rot_delta() {
        Some(d) => (((d * 4.0).round().rem_euclid(4.0)) as i32 & 3) as u8,
        None => 0,
    }
}

// --- leaf-picker shims -----------------------------------------------------------

static RS_DIR_PIC_TRAMP: AtomicUsize = AtomicUsize::new(0);
static ARI_FRAME_TRAMP: AtomicUsize = AtomicUsize::new(0);
static PIPE_GROUP_TRAMP: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // stable home for a rotated orientation: the RotatedSprite shim
    // substitutes a POINTER argument, which must stay valid while the
    // original reads it on this thread
    static ROT_ORI_SLOT: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

// RotatedSprite::getDirectionPictureIndex(uint n, RealOrientation const&):
// returns the (possibly substituted) orientation pointer for r8
extern "C" fn rs_dir_pic_compute(
    _this: *mut core::ffi::c_void,
    _n: u32,
    ori: *const f32,
) -> *const f32 {
    if let Some(d) = frame_rot_delta() {
        if !ori.is_null() {
            let o = unsafe { std::ptr::read(ori) };
            if o.is_finite() {
                return ROT_ORI_SLOT.with(|s| {
                    s.set((o + d).rem_euclid(1.0));
                    s.as_ptr() as *const f32
                });
            }
        }
    }
    ori
}

// AnimationRotationIndex::getFrameIndexForOrientation(RealOrientation by
// value = f32 bits in edx): returns the (possibly rotated) bits for edx
extern "C" fn ari_frame_compute(_this: *mut core::ffi::c_void, bits: u32) -> u32 {
    if let Some(d) = frame_rot_delta() {
        let o = f32::from_bits(bits);
        // RealOrientation is a [0,1) fraction — anything else passes through
        if o.is_finite() && (0.0..1.0).contains(&o) {
            return (o + d).rem_euclid(1.0).to_bits();
        }
    }
    bits
}

// Pipe::getSpriteGroup(byte connection_mask, bool): rotates the 4 cardinal
// connection bits by the camera quadrant; returns the new mask for dl
extern "C" fn pipe_group_compute(_this: *mut core::ffi::c_void, mask: u8, _windowed: u8) -> u8 {
    let q = dir_rot_steps() as u32;
    if q == 0 {
        return mask;
    }
    let m = (mask & 0x0F) as u32;
    ((((m << q) | (m >> (4 - q))) & 0xF) as u8) | (mask & 0xF0)
}

// naked shim: save rcx/rdx/r8/r9/r10/r11 + xmm0-5, call `compute` for the
// substituted argument, restore everything, call the trampoline, then
// restore the caller's registers AGAIN before returning (the caller may
// rely on the substituted register itself being preserved).
//
// stack: entry rsp = 8 mod 16; 6 pushes + sub 0x88 keeps both call sites
// 16-aligned; [rsp..rsp+0x20) is callee shadow space
macro_rules! preserving_shim {
    ($shim:ident, $compute:path, $tramp:path, $apply:literal) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $shim() {
            core::arch::naked_asm!(
                "push rcx",
                "push rdx",
                "push r8",
                "push r9",
                "push r10",
                "push r11",
                "sub rsp, 0x88",
                "movdqu [rsp+0x20], xmm0",
                "movdqu [rsp+0x30], xmm1",
                "movdqu [rsp+0x40], xmm2",
                "movdqu [rsp+0x50], xmm3",
                "movdqu [rsp+0x60], xmm4",
                "movdqu [rsp+0x70], xmm5",
                "call {compute}",
                "movdqu xmm0, [rsp+0x20]",
                "movdqu xmm1, [rsp+0x30]",
                "movdqu xmm2, [rsp+0x40]",
                "movdqu xmm3, [rsp+0x50]",
                "movdqu xmm4, [rsp+0x60]",
                "movdqu xmm5, [rsp+0x70]",
                "mov rcx, [rsp+0xB0]",
                "mov rdx, [rsp+0xA8]",
                "mov r8,  [rsp+0xA0]",
                "mov r9,  [rsp+0x98]",
                "mov r10, [rsp+0x90]",
                "mov r11, [rsp+0x88]",
                $apply,
                "call qword ptr [rip + {tramp}]",
                "mov rcx, [rsp+0xB0]",
                "mov rdx, [rsp+0xA8]",
                "mov r8,  [rsp+0xA0]",
                "mov r9,  [rsp+0x98]",
                "mov r10, [rsp+0x90]",
                "mov r11, [rsp+0x88]",
                "add rsp, 0xB8",
                "ret",
                compute = sym $compute,
                tramp = sym $tramp,
            )
        }
    };
}

preserving_shim!(rs_dir_pic_shim, rs_dir_pic_compute, RS_DIR_PIC_TRAMP, "mov r8, rax");
preserving_shim!(ari_frame_shim, ari_frame_compute, ARI_FRAME_TRAMP, "mov edx, eax");
preserving_shim!(pipe_group_shim, pipe_group_compute, PIPE_GROUP_TRAMP, "mov dl, al");

// --- regular hooks in this group ------------------------------------------------------

// Sprite4Way draws (pipe covers, pumps): rotate the direction argument
fn hooked_sprite_nway4_draw(
    this: *mut core::ffi::c_void,
    queue: *mut core::ffi::c_void,
    pos: *const MapPos,
    dir: u8,
    layer: u8,
) {
    let q = dir_rot_steps();
    let dir = if q != 0 && dir < 16 { (dir + 4 * q) & 0x0F } else { dir };
    unsafe { SpriteNWay4DrawHook.call(this, queue, pos, dir, layer) }
}

// run `f` with every other thread of this process suspended.
//
// enabling a detour is a non-atomic multi-byte write over live code, and
// these targets are hot on the game's update thread — patching them while a
// thread runs mid-prologue crashes. inside `f`: no allocation, no logging,
// no locks (a suspended thread frozen inside malloc would deadlock us)
pub(super) fn with_other_threads_suspended<R>(f: impl FnOnce() -> R) -> R {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
        THREAD_SUSPEND_RESUME,
    };
    let me = unsafe { GetCurrentThreadId() };
    let pid = unsafe { GetCurrentProcessId() };
    // pre-allocate BEFORE suspending anything
    let mut suspended = Vec::with_capacity(256);
    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            let mut te = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            if Thread32First(snap, &mut te).is_ok() {
                loop {
                    if te.th32OwnerProcessID == pid && te.th32ThreadID != me {
                        if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, te.th32ThreadID) {
                            if suspended.len() < suspended.capacity() {
                                SuspendThread(h);
                                suspended.push(h);
                            } else {
                                let _ = CloseHandle(h);
                            }
                        }
                    }
                    te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
                    if Thread32Next(snap, &mut te).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
    }
    let r = f();
    unsafe {
        for h in suspended {
            ResumeThread(h);
            let _ = CloseHandle(h);
        }
    }
    r
}

pub fn install(symbols: &SymbolMap, base: usize) -> Result<()> {
    // phase 1: resolve + initialize (allocates trampolines) — nothing
    // observable changes until enable
    let rs_addr = super::resolve(symbols, base, &offsets::RS_DIR_PIC_INDEX);
    let ari_addr = super::resolve(symbols, base, &offsets::ARI_FRAME_INDEX);
    let nway_addr = super::resolve(symbols, base, &offsets::SPRITE_NWAY4_DRAW);
    let pipe_addr = super::resolve(symbols, base, &offsets::PIPE_GET_SPRITE_GROUP);
    let dsr_addr = super::resolve(symbols, base, &offsets::DQ_DRAW_SCALED_ROTATED);

    let (rs_det, ari_det, pipe_det) = unsafe {
        let rs = retour::RawDetour::new(rs_addr as *const (), rs_dir_pic_shim as *const ())?;
        RS_DIR_PIC_TRAMP.store(rs.trampoline() as *const _ as usize, Ordering::SeqCst);
        let ari = retour::RawDetour::new(ari_addr as *const (), ari_frame_shim as *const ())?;
        ARI_FRAME_TRAMP.store(ari.trampoline() as *const _ as usize, Ordering::SeqCst);
        let pipe = retour::RawDetour::new(pipe_addr as *const (), pipe_group_shim as *const ())?;
        PIPE_GROUP_TRAMP.store(pipe.trampoline() as *const _ as usize, Ordering::SeqCst);
        (rs, ari, pipe)
    };
    unsafe {
        SpriteNWay4DrawHook
            .initialize(std::mem::transmute(nway_addr), hooked_sprite_nway4_draw)?;
        DrawScaledRotatedHook
            .initialize(std::mem::transmute(dsr_addr), super::sprites::hooked_draw_scaled_rotated)?;
    }

    // phase 2: write the patches while no other thread can be executing the
    // target prologues
    let res: Result<()> = with_other_threads_suspended(|| unsafe {
        rs_det.enable()?;
        ari_det.enable()?;
        pipe_det.enable()?;
        SpriteNWay4DrawHook.enable()?;
        DrawScaledRotatedHook.enable()?;
        Ok(())
    });
    res?;

    // the raw detours must live for the process lifetime (dropping unhooks)
    std::mem::forget(rs_det);
    std::mem::forget(ari_det);
    std::mem::forget(pipe_det);
    log::info!("frame-rotation hooks installed");
    Ok(())
}

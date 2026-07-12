// input-related hooks: real cursor position, view-relative walking,
// and cursor->tile picking in the warped view.

use crate::offsets;
use crate::symbols::SymbolMap;
use anyhow::{Context, Result};
use retour::static_detour;
use windows::Win32::Foundation::POINT;

use super::Vec2f;

type FnGetCursorPos = unsafe extern "system" fn(*mut POINT) -> i32;
// static function; struct return via hidden pointer in rcx
type FnComputeWalkDir = unsafe extern "C" fn(*mut Vec2f) -> *mut Vec2f;
// getMapPosition(PixelPosition) — pixel packed as x=low32, y=high32
type FnGetMapPosition = unsafe extern "C" fn(
    *mut core::ffi::c_void,
    *mut core::ffi::c_void,
    u64,
) -> *mut core::ffi::c_void;

static_detour! {
    static GetCursorPosHook: unsafe extern "system" fn(*mut POINT) -> i32;
    static ComputeWalkDirHook: unsafe extern "C" fn(*mut Vec2f) -> *mut Vec2f;
    static GetMapPositionHook: unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, u64) -> *mut core::ffi::c_void;
}

pub fn install(symbols: &SymbolMap, base: usize) -> Result<()> {
    // GetCursorPos is hooked as a passthrough only so real_cursor_pos() can
    // read the true position through the trampoline
    let user32 = unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleA(windows::core::s!("user32.dll"))?
    };
    let addr = unsafe {
        windows::Win32::System::LibraryLoader::GetProcAddress(
            user32,
            windows::core::s!("GetCursorPos"),
        )
    }
    .context("GetCursorPos not found")?;
    unsafe {
        let target: FnGetCursorPos = std::mem::transmute(addr);
        GetCursorPosHook.initialize(target, hooked_get_cursor_pos)?.enable()?;
    }

    let addr = super::resolve(symbols, base, &offsets::COMPUTE_WALK_DIR);
    unsafe {
        let target: FnComputeWalkDir = std::mem::transmute(addr);
        ComputeWalkDirHook.initialize(target, hooked_walk_dir)?.enable()?;
    }

    let addr = super::resolve(symbols, base, &offsets::GET_MAP_POSITION);
    unsafe {
        let target: FnGetMapPosition = std::mem::transmute(addr);
        GetMapPositionHook.initialize(target, hooked_get_map_position)?.enable()?;
    }
    Ok(())
}

// the true cursor position (through the trampoline, bypassing any hook)
pub fn real_cursor_pos() -> POINT {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { GetCursorPosHook.call(&mut pt) };
    pt
}

fn hooked_get_cursor_pos(point: *mut POINT) -> i32 {
    unsafe { GetCursorPosHook.call(point) }
}

// rotate wasd movement by the camera yaw, so "up" walks toward the top of
// the rotated view instead of fixed map-north
fn hooked_walk_dir(ret: *mut Vec2f) -> *mut Vec2f {
    let out = unsafe { ComputeWalkDirHook.call(ret) };
    if !ret.is_null() {
        let (yaw, _p, _z) = crate::camera::get();
        if yaw.abs() > 0.5 {
            let v = unsafe { *ret };
            let a = (-yaw as f64).to_radians();
            let (s, c) = (a.sin(), a.cos());
            unsafe {
                (*ret).x = v.x * c - v.y * s;
                (*ret).y = v.x * s + v.y * c;
            }
        }
    }
    out
}

// un-warp cursor->world picking so hovering targets the right tile in the
// rotated view. only world picking calls this — menus stay untouched
fn hooked_get_map_position(
    this: *mut core::ffi::c_void,
    ret: *mut core::ffi::c_void,
    pixel: u64,
) -> *mut core::ffi::c_void {
    let mut pixel = pixel;
    let x = (pixel & 0xFFFF_FFFF) as u32 as i32;
    let y = (pixel >> 32) as u32 as i32;
    if let Some((nx, ny)) = crate::picking::unwarp(x, y) {
        pixel = ((ny as u32 as u64) << 32) | (nx as u32 as u64);
    }
    unsafe { GetMapPositionHook.call(this, ret, pixel) }
}

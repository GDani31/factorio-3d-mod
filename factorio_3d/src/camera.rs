// camera state and input.
//
// controls:
// - shift + right-drag (or middle-drag): rotate the camera
// - shift + scroll: 3d zoom out / back in
// - shift + scroll IN at closest zoom: first-person mode
//   (mouse steers, ctrl frees the cursor, scroll out to leave)

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
    pub sensitivity: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self { yaw: 0.0, pitch: 90.0, zoom: 1.0, sensitivity: 0.3 }
    }
}

static CAMERA: Mutex<Option<Camera>> = Mutex::new(None);
static FPS_MODE: AtomicBool = AtomicBool::new(false);
static LAST_MOUSE_X: AtomicI32 = AtomicI32::new(0);
static LAST_MOUSE_Y: AtomicI32 = AtomicI32::new(0);
static MOUSE_TRACKING: AtomicBool = AtomicBool::new(false);
static SCROLL_ACCUM: AtomicI32 = AtomicI32::new(0);
static ALT_RMB_DOWN: AtomicBool = AtomicBool::new(false);
static ORIG_WNDPROC: AtomicI64 = AtomicI64::new(0);
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
// game window handle from the swapchain (title search fails in fullscreen)
static GAME_HWND: AtomicI64 = AtomicI64::new(0);

pub fn init() {
    *CAMERA.lock().unwrap() = Some(Camera::default());
    log::info!("[camera] ready (shift+rmb rotates, shift+scroll zooms)");
}

pub fn fps_mode() -> bool {
    FPS_MODE.load(Ordering::Relaxed)
}

// current (yaw, pitch, zoom)
pub fn get() -> (f32, f32, f32) {
    match &*CAMERA.lock().unwrap() {
        Some(c) => (c.yaw, c.pitch, c.zoom),
        None => (0.0, 90.0, 1.0),
    }
}

pub fn set_game_hwnd(hwnd: isize) {
    GAME_HWND.store(hwnd as i64, Ordering::Relaxed);
}

pub fn game_hwnd() -> HWND {
    HWND(GAME_HWND.load(Ordering::Relaxed) as *mut core::ffi::c_void)
}

// subclass the game window once so we see scroll-wheel messages
fn ensure_scroll_hook() {
    if HOOK_INSTALLED.load(Ordering::Relaxed) {
        return;
    }
    let hwnd = game_hwnd();
    if hwnd.0.is_null() {
        return;
    }
    let old = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrA(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_WNDPROC,
            scroll_wndproc as *const () as isize,
        )
    };
    if old != 0 {
        ORIG_WNDPROC.store(old as i64, Ordering::Relaxed);
        HOOK_INSTALLED.store(true, Ordering::Relaxed);
        log::info!("[camera] scroll hook installed");
    }
}

unsafe extern "system" fn scroll_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    const WM_MOUSEWHEEL: u32 = 0x020A;
    if msg == WM_MOUSEWHEEL {
        let delta = (wparam.0 >> 16) as i16;
        SCROLL_ACCUM.fetch_add(delta as i32, Ordering::Relaxed);
        // in first person the wheel belongs to the mod alone (scroll-out exits)
        if FPS_MODE.load(Ordering::Relaxed) {
            return LRESULT(0);
        }
    }
    let orig = ORIG_WNDPROC.load(Ordering::Relaxed) as isize;
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::CallWindowProcA(
            std::mem::transmute(orig),
            hwnd,
            msg,
            wparam,
            lparam,
        )
    }
}

fn key_held(vk: i32) -> bool {
    unsafe {
        windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk) & (0x8000u16 as i16) != 0
    }
}

// poll input once per frame (called from the present hook / render thread)
pub fn poll() {
    ensure_scroll_hook();

    let shift = key_held(0x10);
    let alt = key_held(0x12);
    let rmb = key_held(0x02);
    let mmb = key_held(0x04);
    // shift prevents factorio's own right-click action; mmb alone also rotates
    let rotate_active = (shift && rmb) || mmb;

    // alt + right-click snaps the view back to top-down vanilla (edge-triggered
    // so one click resets once). works in first person too.
    let alt_rmb = alt && rmb;
    if alt_rmb && !ALT_RMB_DOWN.swap(true, Ordering::Relaxed) {
        reset_view();
        return;
    }
    if !alt_rmb {
        ALT_RMB_DOWN.store(false, Ordering::Relaxed);
    }

    if FPS_MODE.load(Ordering::Relaxed) {
        poll_first_person();
        return;
    }

    let point = crate::hooks::input::real_cursor_pos();
    let tracking = MOUSE_TRACKING.load(Ordering::Relaxed);

    if rotate_active {
        if tracking {
            let dx = point.x - LAST_MOUSE_X.load(Ordering::Relaxed);
            let dy = point.y - LAST_MOUSE_Y.load(Ordering::Relaxed);
            if dx != 0 || dy != 0 {
                if let Some(cam) = &mut *CAMERA.lock().unwrap() {
                    // mouse left = rotate right (user preference); only the
                    // input mapping is flipped, yaw itself means the same
                    cam.yaw = (cam.yaw - dx as f32 * cam.sensitivity).rem_euclid(360.0);
                    cam.pitch = (cam.pitch + dy as f32 * cam.sensitivity).clamp(18.0, 90.0);
                }
            }
        } else {
            MOUSE_TRACKING.store(true, Ordering::Relaxed);
        }
        LAST_MOUSE_X.store(point.x, Ordering::Relaxed);
        LAST_MOUSE_Y.store(point.y, Ordering::Relaxed);
    } else {
        MOUSE_TRACKING.store(false, Ordering::Relaxed);
    }

    // shift + scroll = 3d zoom (plain scroll stays the game's own zoom)
    let scroll = SCROLL_ACCUM.swap(0, Ordering::Relaxed);
    if scroll != 0 && shift {
        if let Some(cam) = &mut *CAMERA.lock().unwrap() {
            if scroll > 0 && cam.zoom <= 1.001 {
                // scrolling in at closest 3d zoom -> first person
                FPS_MODE.store(true, Ordering::Relaxed);
                cam.pitch = cam.pitch.clamp(30.0, 60.0);
                log::info!("[camera] first-person ON (ctrl frees cursor, shift+scroll out exits)");
            } else {
                let factor = 1.0 - (scroll as f32 / 120.0) * 0.1;
                // 3d zoom only pulls OUT (zooming in is the game's own zoom)
                cam.zoom = (cam.zoom * factor).clamp(1.0, 3.0);
            }
        }
    }
}

// snap back to the top-down vanilla view (exits first person too)
fn reset_view() {
    FPS_MODE.store(false, Ordering::Relaxed);
    MOUSE_TRACKING.store(false, Ordering::Relaxed);
    if let Some(cam) = &mut *CAMERA.lock().unwrap() {
        cam.yaw = 0.0;
        cam.pitch = 90.0;
        cam.zoom = 1.0;
    }
    log::info!("[camera] view reset to top-down");
}

// first person: classic mouselook (cursor re-centered every frame)
fn poll_first_person() {
    MOUSE_TRACKING.store(false, Ordering::Relaxed);
    let ctrl = key_held(0x11);
    let hwnd = game_hwnd();
    let foreground =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() == hwnd };

    if !ctrl && foreground && !hwnd.0.is_null() {
        unsafe {
            let mut rect = windows::Win32::Foundation::RECT::default();
            if windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect).is_ok() {
                let mut center = windows::Win32::Foundation::POINT {
                    x: (rect.right - rect.left) / 2,
                    y: (rect.bottom - rect.top) / 2,
                };
                let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut center);
                let point = crate::hooks::input::real_cursor_pos();
                let dx = point.x - center.x;
                let dy = point.y - center.y;
                if dx != 0 || dy != 0 {
                    if let Some(cam) = &mut *CAMERA.lock().unwrap() {
                        // mouse-right turns the view right; pitch = look-down
                        // angle (~25 = horizon, 110 = at your feet)
                        cam.yaw = (cam.yaw - dx as f32 * cam.sensitivity).rem_euclid(360.0);
                        cam.pitch = (cam.pitch + dy as f32 * cam.sensitivity).clamp(5.0, 110.0);
                    }
                    let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(
                        center.x, center.y,
                    );
                }
            }
        }
    }

    // scroll out exits back to third person
    let scroll = SCROLL_ACCUM.swap(0, Ordering::Relaxed);
    if scroll < 0 {
        FPS_MODE.store(false, Ordering::Relaxed);
        if let Some(cam) = &mut *CAMERA.lock().unwrap() {
            cam.pitch = cam.pitch.clamp(18.0, 90.0);
            cam.zoom = 1.0;
        }
        log::info!("[camera] first-person OFF");
    }
}

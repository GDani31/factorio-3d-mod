// cursor "un-warp": while the view is rotated, screen pixels no longer map
// 1:1 to world tiles. the getMapPosition hook sends cursor positions through
// unwarp() so world picking hits the right tile. flat gui/menus never call
// that function, so they stay untouched.

use glam::{Mat4, Vec4};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

struct WarpState {
    // inverse of the warp camera's proj*view
    inv_proj_view: Mat4,
    aspect: f32,
    win_w: f32,
    win_h: f32,
}

static STATE: Mutex<Option<WarpState>> = Mutex::new(None);
static ACTIVE: AtomicBool = AtomicBool::new(false);

// publish this frame's warp transform (called by the renderer)
pub fn set_transform(view_proj: &Mat4, aspect: f32, win_w: u32, win_h: u32) {
    *STATE.lock().unwrap() = Some(WarpState {
        inv_proj_view: view_proj.inverse(),
        aspect,
        win_w: win_w as f32,
        win_h: win_h as f32,
    });
    ACTIVE.store(true, Ordering::Relaxed);
}

// back to normal top-down view
pub fn clear() {
    ACTIVE.store(false, Ordering::Relaxed);
}

// map a cursor position (window px) to the vanilla-view position (window px)
// showing the same world point. None = warp off or the ray misses the ground.
pub fn unwarp(screen_x: i32, screen_y: i32) -> Option<(i32, i32)> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let state = STATE.lock().unwrap();
    let state = state.as_ref()?;

    // window px -> ndc
    let ndc_x = (screen_x as f32 / state.win_w) * 2.0 - 1.0;
    let ndc_y = 1.0 - (screen_y as f32 / state.win_h) * 2.0;

    // unproject a ray and intersect it with the ground plane (y = 0)
    let near4 = state.inv_proj_view * Vec4::new(ndc_x, ndc_y, 0.0, 1.0);
    let far4 = state.inv_proj_view * Vec4::new(ndc_x, ndc_y, 1.0, 1.0);
    let near = near4.truncate() / near4.w;
    let far = far4.truncate() / far4.w;
    let dir = far - near;
    if dir.y.abs() < 0.0001 {
        return None;
    }
    let t = -near.y / dir.y;
    if t < 0.0 {
        return None;
    }
    let hit = near + dir * t;

    // plane origin = the camera pivot = the player; the vanilla view spans
    // exactly +-aspect x +-1 in plane units around it. off-screen results are
    // fine (the game extrapolates its linear pixel->map transform).
    let px = state.win_w * (0.5 + hit.x / (2.0 * state.aspect));
    let py = state.win_h * (0.5 - hit.z / 2.0);
    let lim_x = state.win_w * 4.0;
    let lim_y = state.win_h * 4.0;
    Some((px.clamp(-lim_x, lim_x) as i32, py.clamp(-lim_y, lim_y) as i32))
}

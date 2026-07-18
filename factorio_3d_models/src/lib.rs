#![recursion_limit = "256"]
// factorio_3d_models - 3d camera + real 3d models for factorio.
//
// the dll is injected into the running game. it hooks rendering functions
// (found by name in factorio.pdb) to tilt/rotate the camera, and replaces
// entity sprites with real 3d models loaded from the glbs under models/.
//
// module overview:
// - settings       - compile-time constants (colors, sun, spans)
// - tuning         - live-reloaded knobs from f3dm_tuning.txt
// - offsets        - game-version-specific addresses (update after a game update)
// - symbols        - looks up game functions in factorio.pdb
// - camera         - camera state + mouse/keyboard input
// - picking        - maps the cursor back to the right tile while warped
// - hooks          - all detours into the game's code (frame, input,
//                    machines, items, wires + mem/getters helpers)
// - entities       - registry of replaced entities seen by the draw hooks
// - models         - maps prototype names to glbs under models/, lazy loading
// - gltf_model     - loads one glb (nodes, meshes, textures, TRS animation)
// - model_renderer - d3d11 pipeline that draws the 3d model instances
// - warp           - re-renders the frame as a tilted ground plane
// - renderer       - per-frame orchestration
// - util           - AtomicF32 + the memo cache helper

mod camera;
mod entities;
mod gltf_model;
mod hooks;
mod model_renderer;
mod models;
mod offsets;
mod picking;
mod renderer;
mod settings;
mod symbols;
mod tuning;
mod util;
mod warp;

use windows::Win32::Foundation::{BOOL, TRUE};
use windows::Win32::System::Console::AllocConsole;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

// called by windows when the dll is loaded into factorio
#[unsafe(no_mangle)]
extern "system" fn DllMain(
    _hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        std::thread::spawn(|| {
            if let Err(e) = start() {
                eprintln!("[factorio_3d_models] failed to start: {e:#}");
            }
        });
    }
    TRUE
}

// a panic inside a hooked extern "C" function aborts the whole game
// (0xc0000409) and the message dies with the console. this hook runs BEFORE
// the abort: it writes message + location + backtrace both to the log and to
// its own flushed crash file, so the crash is diagnosable from disk
fn install_panic_hook() {
    use std::io::Write;
    static PANICS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::panic::set_hook(Box::new(|info| {
        let n = PANICS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n >= 20 {
            return; // a caught per-frame panic would spam forever
        }
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("PANIC #{n} [{:?}]: {info}\nbacktrace:\n{bt}", std::thread::current().id());
        log::error!("{msg}");
        if let Some(dir) = log_file_path().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("factorio_3d_models_crash.txt"))
            {
                let _ = writeln!(f, "[{:?}] {msg}\n", std::time::SystemTime::now());
                let _ = f.flush();
                let _ = f.sync_all();
            }
        }
    }));
}

// set up logging, load the model, find the game's functions, install hooks
fn start() -> anyhow::Result<()> {
    unsafe { AllocConsole()? };
    install_panic_hook();

    // log to a console AND a file (the console closes with the game)
    let log_path = log_file_path();
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Some(path) = &log_path {
        if let Ok(file) = std::fs::File::create(path) {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    builder.init();

    log::info!("=== factorio_3d_models v{} loaded ===", env!("CARGO_PKG_VERSION"));
    if let Some(path) = &log_path {
        log::info!("log file: {}", path.display());
    }

    // scan the model registry before hooking — if it fails the camera still works
    models::init();
    tuning::init();

    let symbols = symbols::find_game_functions()?;
    log::info!("found {} game functions in the pdb", symbols.len());

    camera::init();
    hooks::install(&symbols)?;
    log::info!("all hooks installed");
    Ok(())
}

// directory of the injected dll (models/ lives next to it, or two dirs up)
pub(crate) fn dll_dir() -> Option<std::path::PathBuf> {
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameA, GetModuleHandleA};
    let handle =
        unsafe { GetModuleHandleA(windows::core::s!("factorio_3d_models.dll")) }.ok()?;
    let mut buf = [0u8; 1024];
    let len = unsafe { GetModuleFileNameA(handle, &mut buf) } as usize;
    if len == 0 {
        return None;
    }
    let path = std::path::PathBuf::from(String::from_utf8_lossy(&buf[..len]).into_owned());
    path.parent().map(|p| p.to_path_buf())
}

// log file: %APPDATA%\Factorio\factorio_3d_models.log (temp dir as fallback)
fn log_file_path() -> Option<std::path::PathBuf> {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let dir = std::path::PathBuf::from(appdata).join("Factorio");
        if dir.is_dir() {
            return Some(dir.join("factorio_3d_models.log"));
        }
    }
    Some(std::env::temp_dir().join("factorio_3d_models.log"))
}

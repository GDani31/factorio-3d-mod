// factorio_3d - a 3d camera mod for factorio.
//
// the dll is injected into the running game. it hooks rendering functions
// (found by name in factorio.pdb) to tilt/rotate the camera, stand buildings
// up as billboards, and lift belts/rails off the ground.
//
// module overview:
// - settings   - all tunable numbers in one place
// - offsets    - game-version-specific addresses (update after a game update)
// - symbols    - looks up game functions in factorio.pdb
// - camera     - camera state + mouse/keyboard input
// - picking    - maps the cursor back to the right tile while warped
// - hooks      - all detours into the game's code
// - billboards - per-entity sprite rects recorded each frame
// - capture    - offscreen gpu targets the object/belt layers go into
// - warp       - the d3d11 pipeline that re-renders the frame in 3d
// - renderer   - per-frame orchestration

mod billboards;
mod camera;
mod capture;
mod hooks;
mod offsets;
mod picking;
mod renderer;
mod settings;
mod symbols;
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
                eprintln!("[factorio_3d] failed to start: {e:#}");
            }
        });
    }
    TRUE
}

// set up logging, find the game's functions, install all hooks
fn start() -> anyhow::Result<()> {
    unsafe { AllocConsole()? };

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

    log::info!("=== factorio_3d v{} loaded ===", env!("CARGO_PKG_VERSION"));
    if let Some(path) = &log_path {
        log::info!("log file: {}", path.display());
    }

    let symbols = symbols::find_game_functions()?;
    log::info!("found {} game functions in the pdb", symbols.len());

    camera::init();
    hooks::install(&symbols)?;
    log::info!("all hooks installed");
    Ok(())
}

// log file: %APPDATA%\Factorio\factorio_3d.log (temp dir as fallback)
fn log_file_path() -> Option<std::path::PathBuf> {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let dir = std::path::PathBuf::from(appdata).join("Factorio");
        if dir.is_dir() {
            return Some(dir.join("factorio_3d.log"));
        }
    }
    Some(std::env::temp_dir().join("factorio_3d.log"))
}

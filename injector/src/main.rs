// tiny injector: finds the running factorio process and loads our dll into it.
// usage: start factorio, then run `cargo run --release -p injector`
// (or `inject.exe [path\to\factorio_3d.dll]`).

use anyhow::{Context, Result};
use dll_syringe::{Syringe, process::OwnedProcess};
use std::path::PathBuf;

fn main() -> Result<()> {
    println!("=== factorio_3d injector ===");

    let dll = find_dll()?;
    println!("DLL: {}", dll.display());

    let process = OwnedProcess::find_first_by_name("factorio.exe")
        .context("factorio is not running. start the game first, then run this again.")?;
    println!("found factorio.exe, injecting...");

    Syringe::for_process(process)
        .inject(dll)
        .context("injection failed")?;

    println!("done! a console window should appear inside factorio.");
    Ok(())
}

// dll path: first cli arg, or next to this exe, or in target/
fn find_dll() -> Result<PathBuf> {
    if let Some(path) = std::env::args().nth(1) {
        return Ok(PathBuf::from(path));
    }
    let exe_dir = std::env::current_exe()?
        .parent()
        .context("no exe dir")?
        .to_path_buf();
    let candidates = [
        exe_dir.join("factorio_3d.dll"),
        PathBuf::from("target/release/factorio_3d.dll"),
        PathBuf::from("target/debug/factorio_3d.dll"),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.canonicalize()?);
        }
    }
    anyhow::bail!("factorio_3d.dll not found — build it first: cargo build --release")
}

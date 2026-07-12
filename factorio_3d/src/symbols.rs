// finds the game's internal functions by reading factorio.pdb.
// factorio ships debug symbols on windows, so functions can be located by
// name instead of hardcoded addresses — that way most hooks survive updates.

use anyhow::{Context, Result};
use pdb::FallibleIterator;
use std::collections::HashMap;
use std::path::PathBuf;
use windows::Win32::System::LibraryLoader::GetModuleHandleA;

// symbol name -> absolute address in the running process
pub type SymbolMap = HashMap<String, usize>;

// base address of factorio.exe in memory
pub fn base_address() -> Result<usize> {
    let handle = unsafe { GetModuleHandleA(None)? };
    Ok(handle.0 as usize)
}

fn find_pdb_path() -> Result<PathBuf> {
    let exe_dir = std::env::current_exe()
        .context("failed to get exe path")?
        .parent()
        .context("failed to get exe dir")?
        .to_path_buf();
    let candidates = [
        exe_dir.join("factorio.pdb"),
        exe_dir.join("bin/x64/factorio.pdb"),
        PathBuf::from("factorio.pdb"),
        PathBuf::from("bin/x64/factorio.pdb"),
    ];
    for path in &candidates {
        if path.exists() {
            log::info!("pdb: {}", path.display());
            return Ok(path.clone());
        }
    }
    anyhow::bail!("factorio.pdb not found (searched {candidates:?})")
}

// scan the pdb for every function listed in offsets::ALL
pub fn find_game_functions() -> Result<SymbolMap> {
    let base = base_address()?;
    log::info!("factorio.exe base: 0x{base:X}");

    let pdb_path = find_pdb_path()?;
    let file = std::fs::File::open(&pdb_path)
        .with_context(|| format!("failed to open {}", pdb_path.display()))?;
    let mut pdb = pdb::PDB::open(file)?;
    let symbol_table = pdb.global_symbols()?;
    let address_map = pdb.address_map()?;

    let mut symbols = SymbolMap::new();
    let mut iter = symbol_table.iter();
    while let Some(symbol) = iter.next()? {
        if let Ok(pdb::SymbolData::Public(data)) = symbol.parse() {
            let name = data.name.to_string().to_string();
            let wanted = crate::offsets::ALL
                .iter()
                .any(|gf| !gf.symbol.is_empty() && name.contains(gf.symbol));
            if wanted {
                if let Some(rva) = data.offset.to_rva(&address_map) {
                    symbols.insert(name, base + rva.0 as usize);
                }
            }
        }
    }
    Ok(symbols)
}

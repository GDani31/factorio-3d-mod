//! PDB Explorer - search Factorio's debug symbols for rendering functions.
//!
//! Usage:
//!   pdb-explorer <path-to-factorio.pdb> [search-term]
//!
//! Examples:
//!   pdb-explorer factorio.pdb render
//!   pdb-explorer factorio.pdb sprite
//!   pdb-explorer factorio.pdb draw
//!   pdb-explorer factorio.pdb opengl
//!   pdb-explorer factorio.pdb camera

use anyhow::{Context, Result};
use clap::Parser;
use pdb::FallibleIterator;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pdb-explorer")]
#[command(about = "Explore Factorio's PDB symbols to find hookable render functions")]
struct Args {
    /// Path to factorio.pdb
    pdb_path: PathBuf,

    /// Search term to filter symbols (case-insensitive substring match)
    #[arg(default_value = "render")]
    search: String,

    /// Maximum number of results to show
    #[arg(short, long, default_value = "100")]
    max: usize,

    /// Show all symbol types (not just public functions)
    #[arg(short, long)]
    all: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    println!("Opening PDB: {}", args.pdb_path.display());
    let file = std::fs::File::open(&args.pdb_path)
        .with_context(|| format!("Cannot open: {}", args.pdb_path.display()))?;

    let mut pdb = pdb::PDB::open(file)?;
    let symbol_table = pdb.global_symbols()?;
    let address_map = pdb.address_map()?;

    let search_lower = args.search.to_lowercase();
    let mut matches: Vec<(String, u32)> = Vec::new();

    println!(
        "Searching for symbols matching \"{}\"...\n",
        args.search
    );

    let mut iter = symbol_table.iter();
    while let Some(symbol) = iter.next()? {
        match symbol.parse() {
            Ok(pdb::SymbolData::Public(data)) => {
                let name = data.name.to_string().to_string();
                if name.to_lowercase().contains(&search_lower) {
                    let rva = data
                        .offset
                        .to_rva(&address_map)
                        .map(|r| r.0)
                        .unwrap_or(0);
                    matches.push((name, rva));
                }
            }
            Ok(pdb::SymbolData::Procedure(data)) if args.all => {
                let name = data.name.to_string().to_string();
                if name.to_lowercase().contains(&search_lower) {
                    let rva = data
                        .offset
                        .to_rva(&address_map)
                        .map(|r| r.0)
                        .unwrap_or(0);
                    matches.push((name, rva));
                }
            }
            _ => {}
        }
    }

    matches.sort_by(|a, b| a.0.cmp(&b.0));

    let total = matches.len();
    let shown = matches.iter().take(args.max);

    println!("{:<12} Symbol", "RVA");
    println!("{:-<12} {:-<60}", "", "");

    for (name, rva) in shown {
        println!("0x{rva:08X}  {name}");
    }

    println!("\n--- {total} symbols found matching \"{}\" ---", args.search);
    if total > args.max {
        println!("(showing first {}, use --max to see more)", args.max);
    }

    Ok(())
}

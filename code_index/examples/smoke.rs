//! Smoke test: chunk every .rs / .py / .pyi file under a path and print
//! a one-line summary per file plus a per-chunk listing.
//!
//!   cargo run -p code_index --example smoke -- <path>
//!   cargo run -p code_index --example smoke
//!
//! With no argument, scans the current directory.

use std::path::Path;

use code_index::chunker::{Chunker, SupportedLanguage};

fn main() -> anyhow::Result<()> {
    let target = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let mut total = 0;
    walk(Path::new(&target), &mut total)?;
    println!("total: {total} chunks");
    Ok(())
}

fn walk(path: &Path, total: &mut usize) -> anyhow::Result<()> {
    if path.is_file() {
        return chunk_file(path, total);
    }
    for entry in std::fs::read_dir(path)? {
        let p = entry?.path();
        // Skip directories that almost never have source we want.
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if matches!(name, "target" | ".git" | ".jj" | "node_modules" | ".venv") {
                continue;
            }
        }
        if p.is_dir() {
            walk(&p, total)?;
        } else {
            chunk_file(&p, total)?;
        }
    }
    Ok(())
}

fn chunk_file(path: &Path, total: &mut usize) -> anyhow::Result<()> {
    let Some(_lang) = SupportedLanguage::from_path(path) else {
        return Ok(());
    };
    let chunker = Chunker::for_path(path).expect("supported language returns Some")?;
    let src = std::fs::read(path)?;
    let chunks = chunker.extract(&src, path)?;
    println!("{}: {} chunks", path.display(), chunks.len());
    for c in &chunks {
        println!(
            "  {:?} {} (lines {}-{})",
            c.kind, c.name, c.lines.start, c.lines.end
        );
    }
    *total += chunks.len();
    Ok(())
}

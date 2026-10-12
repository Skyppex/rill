//! Embeds the core library: every `.rill` file under `core/` becomes part
//! of the compiler, so every program has it wherever it is built.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("core");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    let mut out = String::from(
        "/// The core library: each module's name (its path under `core/`,\n\
         /// without `.rill`) and its source.\n\
         pub static FILES: &[(&str, &str)] = &[\n",
    );
    for path in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path
            .strip_prefix(&root)
            .expect("under core/")
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        let _ = writeln!(
            out,
            "    ({name:?}, include_str!({:?})),",
            path.display().to_string()
        );
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("set by cargo")).join("core.rs");
    std::fs::write(dest, out).expect("OUT_DIR is writable");
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rill") {
            out.push(path);
        }
    }
}

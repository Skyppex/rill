//! Embeds the core library: every `.rill` file under `core/` becomes part
//! of the compiler, so every program has it wherever it is built. Also
//! records the commit the compiler was built from, for `rill --version`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    commit();
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

/// Sets `RILL_COMMIT`: the commit being built, from `RILL_COMMIT` if it is
/// set (as Nix does, having no `.git`), else from git, with `-dirty` if the
/// files differ from it. In a colocated jj repository that is the parent of
/// the working copy. `unknown` without either.
fn commit() {
    println!("cargo:rerun-if-env-changed=RILL_COMMIT");
    let commit = std::env::var("RILL_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(from_git)
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RILL_COMMIT={commit}");
}

fn from_git() -> Option<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(manifest)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let head = git(&["rev-parse", "HEAD"])?;
    // Whether the files differ from the commit can change with any edit, so
    // look again whenever something that is built changes.
    for path in [
        ".git/HEAD",
        ".git/index",
        "src",
        "core",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        let path = manifest.join(path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    Some(if dirty { format!("{head}-dirty") } else { head })
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

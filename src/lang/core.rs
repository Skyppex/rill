//! The core library: Rill modules built into the compiler, imported with
//! `@`, as in `import "@gen/osc"`. Their sources are the files under
//! `core/` in the repository, embedded when the compiler is built, so every
//! program has the same `core` wherever it runs and nothing is read from
//! disk.

include!(concat!(env!("OUT_DIR"), "/core.rs"));

/// The source of the core module `name`, as in `"gen/osc"` (without `@`).
pub fn module(name: &str) -> Option<&'static str> {
    FILES.iter().find(|(n, _)| *n == name).map(|(_, src)| *src)
}

/// Every core module's name, as imported: `"@gen/osc"`.
pub fn names() -> impl Iterator<Item = String> {
    FILES.iter().map(|(n, _)| format!("@{n}"))
}

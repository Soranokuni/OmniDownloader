//! Embed a UAC manifest that says "run as the invoker". Without one, Windows'
//! installer detection elevates any executable named "...Setup..." at launch:
//! `--pack` and `/?` would need an administrator, and the unelevated parent that
//! opens the browser after the elevated child (P13.1b) would never exist.
//! Linker switches only; the full manifest (common controls, DPI) is P13.1c's.

fn main() {
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if msvc {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker'");
        // Load the DLLs this executable imports from System32 only, never from
        // the folder it was downloaded to (LOAD_LIBRARY_SEARCH_SYSTEM32).
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
    println!("cargo:rerun-if-changed=build.rs");
}

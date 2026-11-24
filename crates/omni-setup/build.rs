//! Embed a UAC manifest that says "run as the invoker". Without one, Windows'
//! installer detection elevates any executable named "...Setup..." at launch:
//! `--pack` and `/?` would need an administrator, and the unelevated parent that
//! opens the browser after the elevated child (P13.1b) would never exist.
//! The pages (P13.1c) are task dialogs, which exist only in common controls 6:
//! the manifest names that assembly too. Per-monitor DPI awareness is set by
//! an API call when the pages start, not here. Linker switches only.
//! It also sets `OMNI_GIT_SHA`, the commit `/?` names on its first line (P14.4).

include!("../../build_support/git_sha.rs");

fn main() {
    emit_git_sha();
    println!("cargo:rerun-if-changed=../../build_support/git_sha.rs");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if msvc {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker'");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' \
             version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
        // Load the DLLs this executable imports from System32 only, never from
        // the folder it was downloaded to (LOAD_LIBRARY_SEARCH_SYSTEM32).
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
    println!("cargo:rerun-if-changed=build.rs");
}

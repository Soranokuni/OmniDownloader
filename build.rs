//! `omni-ingest --version` names the commit it was built from (P14.4):
//! `OMNI_GIT_SHA`, read from the checkout without running git.

include!("build_support/git_sha.rs");

fn main() {
    emit_git_sha();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support/git_sha.rs");
}

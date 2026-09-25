//! The start-up encoder self-test against the real toolchain.

use std::path::{Path, PathBuf};

use omni_broadcast::selftest::encoder_self_test;

fn bin(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

#[tokio::test]
async fn the_installed_toolchain_makes_a_compliant_file() {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) else {
        eprintln!("WARNING: skipping encoder self-test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let report = encoder_self_test(&ffmpeg, &ffprobe, &bmx, dir.path())
        .await
        .unwrap_or_else(|e| panic!("the self-test failed on the installed tools: {e:#}"));
    assert!(report.elapsed.as_secs() < 60, "took {:?}", report.elapsed);
    // Leaves nothing behind in temp.
    assert!(!dir.path().join("selftest").exists());
}

#[tokio::test]
async fn a_broken_encoder_fails_the_test_and_says_which_step() {
    let (Some(ffmpeg), Some(ffprobe)) = (bin("ffmpeg"), bin("ffprobe")) else {
        eprintln!("WARNING: skipping encoder self-test -- ffmpeg/ffprobe not in bin/");
        return;
    };
    let dir = tempfile::tempdir().unwrap();

    // A bmxtranswrap that is not there: the transcode succeeds, the rewrap
    // must be what fails.
    let err = encoder_self_test(&ffmpeg, &ffprobe, &dir.path().join("missing-bmx.exe"), dir.path())
        .await
        .err()
        .expect("a missing rewrapper must fail the self-test");
    assert!(format!("{err}").contains("RDD9 rewrap failed"), "{err}");

    // An ffmpeg that is not ffmpeg at all.
    let fake = dir.path().join("ffmpeg.exe");
    std::fs::write(&fake, b"").unwrap();
    let err = encoder_self_test(&fake, &ffprobe, &fake, dir.path())
        .await
        .err()
        .expect("an unusable ffmpeg must fail the self-test");
    assert!(format!("{err:#}").contains("test clip"), "{err:#}");
}

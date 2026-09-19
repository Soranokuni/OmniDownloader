//! SMPTE RDD9 OP1a re-wrap with bmxtranswrap (plan P1.3, P1.7).
//!
//! ffmpeg's MXF muxer produces a file most tools accept; Dalet Galaxy wants
//! RDD9 OP1a specifically, with the timecode rate declared. bmxtranswrap
//! rewraps the essence without re-encoding, so this stage is fast and lossless —
//! but it still needs a timeout and a process-tree kill like any other tool
//! (defect D-03).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use omni_core::process::{run, RunOpts};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Rewrapping copies essence rather than re-encoding, so it runs at disk speed.
/// Ten minutes is generous for anything the queue accepts and still bounded.
const DEFAULT_REWRAP_TIMEOUT: Duration = Duration::from_secs(600);

/// Build the bmxtranswrap argument vector.
///
/// Pure, so the compliance tests assert the flags this process will actually
/// run (plan P0.4). RDD9 OP1a at 25 fps is what Dalet ingests; anything else is
/// rejected or mis-plays.
pub fn build_args(source_mxf: &Path, output_mxf: &Path) -> Vec<String> {
    vec![
        "-t".into(),
        "rdd9".into(),
        "--tc-rate".into(),
        "25".into(),
        "-o".into(),
        output_mxf.to_string_lossy().into_owned(),
        source_mxf.to_string_lossy().into_owned(),
    ]
}

/// Build the argument vector with a clip name.
///
/// `--clip` sets the MXF material package name, which Dalet shows as the asset
/// title. Without it the operator sees the filename only.
pub fn build_args_with_clip(source_mxf: &Path, output_mxf: &Path, clip: &str) -> Vec<String> {
    let mut args = build_args(source_mxf, output_mxf);
    // Insert before the trailing input path: bmxtranswrap treats the last
    // argument as the source.
    let input = args.pop().expect("build_args always ends with the source");
    args.push("--clip".into());
    args.push(clip.to_string());
    args.push(input);
    args
}

pub struct Rewrapper {
    bmxtranswrap_path: PathBuf,
}

impl Rewrapper {
    pub fn new<P: AsRef<Path>>(bmxtranswrap_path: P) -> Self {
        Self {
            bmxtranswrap_path: bmxtranswrap_path.as_ref().to_path_buf(),
        }
    }

    pub async fn rewrap(
        &self,
        job_id: i64,
        intermediate_mxf: &Path,
        temp_dir: &Path,
    ) -> Result<PathBuf> {
        self.rewrap_with_clip(job_id, intermediate_mxf, temp_dir, None, None)
            .await
    }

    /// Rewrap to RDD9 OP1a, optionally naming the material package.
    pub async fn rewrap_with_clip(
        &self,
        job_id: i64,
        intermediate_mxf: &Path,
        temp_dir: &Path,
        clip: Option<&str>,
        cancel: Option<CancellationToken>,
    ) -> Result<PathBuf> {
        tokio::fs::create_dir_all(temp_dir).await?;
        let final_temp_mxf = temp_dir.join(format!("{job_id}_output.mxf"));

        if final_temp_mxf.exists() {
            // A leftover from a previous attempt; bmxtranswrap will not
            // overwrite it, and a stale file here would be delivered as if new.
            let _ = tokio::fs::remove_file(&final_temp_mxf).await;
        }

        let args = match clip {
            Some(name) => build_args_with_clip(intermediate_mxf, &final_temp_mxf, name),
            None => build_args(intermediate_mxf, &final_temp_mxf),
        };

        info!("Job #{job_id}: bmxtranswrap RDD9 OP1a re-wrap");

        let mut opts = RunOpts::new(DEFAULT_REWRAP_TIMEOUT);
        if let Some(token) = cancel {
            opts = opts.with_cancel(token);
        }

        let outcome = run(&self.bmxtranswrap_path, &args, opts)
            .await
            .with_context(|| {
                format!("Could not run bmxtranswrap at {:?}", self.bmxtranswrap_path)
            })?;

        if outcome.timed_out {
            return Err(anyhow!(
                "REWRAP_TIMEOUT: bmxtranswrap exceeded {} s for job #{job_id}",
                DEFAULT_REWRAP_TIMEOUT.as_secs()
            ));
        }
        outcome.ok_or_err("bmxtranswrap").map_err(|e| e.context("REWRAP_FAILED"))?;

        if !final_temp_mxf.exists() {
            return Err(anyhow!(
                "REWRAP_FAILED: bmxtranswrap reported success but produced no file at {final_temp_mxf:?}"
            ));
        }

        Ok(final_temp_mxf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_stays_the_final_argument_when_a_clip_name_is_added() {
        // bmxtranswrap treats the last argument as the input; putting --clip
        // after it would make the clip name the source file.
        let args = build_args_with_clip(
            Path::new("C:/temp/7_transcode.mxf"),
            Path::new("C:/temp/7_output.mxf"),
            "3_PAPADAKI_KNICKS",
        );
        assert_eq!(args.last().unwrap(), "C:/temp/7_transcode.mxf");
        let i = args.iter().position(|a| a == "--clip").expect("--clip");
        assert_eq!(args[i + 1], "3_PAPADAKI_KNICKS");
        assert!(i + 2 < args.len(), "--clip must not be the final pair");
    }
}

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

pub struct TranscodeProgress {
    pub percent: f64,
    pub speed: String,
    pub eta: String,
}

pub struct Transcoder {
    ffmpeg_path: PathBuf,
    ffprobe_path: PathBuf,
}

impl Transcoder {
    pub fn new<P: AsRef<Path>, Q: AsRef<Path>>(ffmpeg_path: P, ffprobe_path: Q) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.as_ref().to_path_buf(),
            ffprobe_path: ffprobe_path.as_ref().to_path_buf(),
        }
    }

    pub async fn get_media_info(&self, media_path: &Path) -> Result<(f64, usize)> {
        let mut cmd = Command::new(&self.ffprobe_path);
        cmd.args([
            "-v", "error",
            "-show_entries", "format=duration:stream=codec_type,channels",
            "-of", "json",
            media_path.to_string_lossy().as_ref(),
        ]);

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let output = cmd.output().await.with_context(|| "Failed running ffprobe")?;
        if !output.status.success() {
            return Ok((0.0, 0));
        }

        let json: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        let duration = json
            .pointer("/format/duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let mut channels = 0;
        if let Some(streams) = json.pointer("/streams").and_then(|v| v.as_array()) {
            for stream in streams {
                if stream.get("codec_type").and_then(|v| v.as_str()) == Some("audio") {
                    channels = stream.get("channels").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    break;
                }
            }
        }

        Ok((duration, channels))
    }

    pub async fn transcode<F>(
        &self,
        job_id: i64,
        input_path: &Path,
        temp_dir: &Path,
        mut on_progress: F,
    ) -> Result<(PathBuf, f64)>
    where
        F: FnMut(TranscodeProgress) + Send + 'static,
    {
        tokio::fs::create_dir_all(temp_dir).await?;
        let (duration, channels) = self.get_media_info(input_path).await.unwrap_or((0.0, 0));

        let out_filename = format!("{}_transcode.mxf", job_id);
        let intermediate_mxf = temp_dir.join(&out_filename);
        if intermediate_mxf.exists() {
            let _ = tokio::fs::remove_file(&intermediate_mxf).await;
        }

        let mut cmd = Command::new(&self.ffmpeg_path);
        cmd.args([
            "-y",
            "-threads", "0",
            "-i", input_path.to_string_lossy().as_ref(),
            "-f", "lavfi",
            "-i", "anullsrc=r=48000:cl=mono",
        ]);

        // EBU R48 Broadcast Audio Mapping:
        // Ch 1 (Left), Ch 2 (Right), Ch 3-8 (Silent mono)
        let mut filter_complex = String::new();
        let mut audio_maps: Vec<&str> = Vec::new();

        if channels == 0 {
            // Pure silence for all 8 channels
            for _ in 0..8 {
                audio_maps.extend_from_slice(&["-map", "1:a"]);
            }
        } else if channels == 1 {
            // Mono: copy to L & R
            filter_complex = "[0:a:0]asplit=2[l][r]".into();
            audio_maps.extend_from_slice(&["-map", "[l]", "-map", "[r]"]);
            for _ in 0..6 {
                audio_maps.extend_from_slice(&["-map", "1:a"]);
            }
        } else {
            // Stereo or multi-channel: pan first two into L and R
            filter_complex = "[0:a:0]pan=mono|c0=c0[l];[0:a:0]pan=mono|c0=c1[r]".into();
            audio_maps.extend_from_slice(&["-map", "[l]", "-map", "[r]"]);
            for _ in 0..6 {
                audio_maps.extend_from_slice(&["-map", "1:a"]);
            }
        }

        if !filter_complex.is_empty() {
            cmd.args(["-filter_complex", &filter_complex]);
        }

        // Map video stream
        cmd.args(["-map", "0:v"]);

        // Append mapped audio channels
        for map in audio_maps {
            cmd.arg(map);
        }

        // Sony XDCAM Long GOP HD422 PAL 1080i50 Video Parameters
        cmd.args([
            "-vf", "scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black,tinterlace=mode=interleave_top:flags=vlpf,format=yuv422p",
            "-sws_flags", "bilinear",
            "-c:v", "mpeg2video",
            "-b:v", "50M",
            "-minrate", "50M",
            "-maxrate", "50M",
            "-bufsize", "17825792",
            "-profile:v", "0",
            "-level:v", "2",
            "-pix_fmt", "yuv422p",
            "-g", "12",
            "-bf", "2",
            "-flags", "+ildct+ilme",
            "-trellis", "0",
            "-top", "1",
            "-r", "25",
            "-aspect", "16:9",
            // Audio output format
            "-c:a", "pcm_s24le",
            "-ar", "48000",
            "-shortest",
            intermediate_mxf.to_string_lossy().as_ref(),
        ]);

        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        info!("Starting FFmpeg XDCAM HD422 broadcast transcode for Job #{}...", job_id);
        let mut child = cmd.spawn().with_context(|| format!("Failed to spawn ffmpeg at {:?}", self.ffmpeg_path))?;

        let stderr = child.stderr.take().ok_or_else(|| anyhow!("Failed to capture ffmpeg stderr"))?;
        let mut reader = BufReader::new(stderr).lines();

        let time_regex = Regex::new(r"time=(\d+):(\d+):(\d+\.\d+)").expect("Valid regex");

        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(caps) = time_regex.captures(&line) {
                if duration > 0.0 {
                    let hours: f64 = caps[1].parse().unwrap_or(0.0);
                    let mins: f64 = caps[2].parse().unwrap_or(0.0);
                    let secs: f64 = caps[3].parse().unwrap_or(0.0);
                    let current_time = hours * 3600.0 + mins * 60.0 + secs;
                    let percent = ((current_time / duration) * 100.0).min(99.9);

                    on_progress(TranscodeProgress {
                        percent,
                        speed: "Transcoding".into(),
                        eta: "--:--".into(),
                    });
                }
            }
        }

        let status = child.wait().await?;
        if !status.success() || !intermediate_mxf.exists() {
            return Err(anyhow!("FFmpeg transcode failed with exit status: {:?}", status.code()));
        }

        Ok((intermediate_mxf, duration))
    }
}

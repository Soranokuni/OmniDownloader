//! The one place that says which ffmpeg, ffprobe and bmxtranswrap builds this
//! station runs (plan P9.3).
//!
//! These tools are the on-air encoder and muxer. A new release can change
//! rate control, the MXF wrap or the audio layout, so they are never taken
//! "latest" and never updated by a scheduled task: the admin panel installs the
//! exact build below, checked against the SHA-256 recorded here, or nothing.
//! Moving to another build is a code change: new pin, new validation.

/// One file the archive is allowed to give us.
#[derive(Debug, Clone)]
pub struct PinMember {
    /// The installed name in `bin/`.
    pub exe: &'static str,
    /// The archive entry is this path or ends with `/` + this path. Only the
    /// last component is ever used as a file name on disk.
    pub entry_suffix: &'static str,
    /// SHA-256 of the extracted file, lowercase hex.
    pub sha256: &'static str,
}

/// One archive, the members taken from it, and how to prove the result runs.
#[derive(Debug, Clone)]
pub struct ToolPin {
    /// Names the staged folder (`bin/<tool>.staged/`) and the install.
    pub tool: &'static str,
    pub version: &'static str,
    pub url: &'static str,
    /// Exact size of the archive; the download is capped at it.
    pub size: u64,
    /// `None` where the release publishes no archive hash; the members' hashes
    /// are then the authority.
    pub archive_sha256: Option<&'static str>,
    pub members: &'static [PinMember],
    /// Which installed member runs for the version probe.
    pub probe_exe: &'static str,
    pub probe_args: &'static [&'static str],
    /// The probe's output must contain this. Empty: no probe (tests only).
    pub probe_contains: &'static str,
    /// The needle must be in the first non-empty line of the output, not just anywhere in it.
    pub probe_first_line: bool,
    /// The exe needs the VC++ 2015-2022 runtime DLLs next to it or in System32.
    pub needs_vcrt: bool,
}

pub const VCRT_DLLS: &[&str] = &["msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"];

pub const PINS: &[ToolPin] = &[
    ToolPin {
        tool: "ffmpeg",
        version: "9.0.2",
        url: "https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-full_build.zip",
        size: 257_736_868,
        archive_sha256: Some("759d0a9831c436a0eb331ad36f236c06cb04aaa0005da46571f0e9d3d9206f6b"),
        members: &[
            PinMember {
                exe: "ffmpeg.exe",
                entry_suffix: "bin/ffmpeg.exe",
                sha256: "589e50b766d251afdf181dd664d40bd94407e200b019989fd7468c7d118a28d0",
            },
            PinMember {
                exe: "ffprobe.exe",
                entry_suffix: "bin/ffprobe.exe",
                sha256: "02264dfa4f56baaa31e443ece8aa16d397f6b008200e7fd184d22b9009e75fd3",
            },
        ],
        probe_exe: "ffmpeg.exe",
        probe_args: &["-version"],
        probe_contains: "9.0.2",
        probe_first_line: true,
        needs_vcrt: false,
    },
    ToolPin {
        tool: "bmxtranswrap",
        version: "1.6.0",
        url: "https://github.com/bbc/bmx/releases/download/v1.6/bmx-win64-binary-1.6.zip",
        size: 2_508_475,
        archive_sha256: None,
        members: &[PinMember {
            exe: "bmxtranswrap.exe",
            entry_suffix: "bmxtranswrap.exe",
            sha256: "78257c8e80636e1a3356d36a4a75f6caf5272378bc68026097f462d2c5928aa1",
        }],
        probe_exe: "bmxtranswrap.exe",
        probe_args: &["-v"],
        probe_contains: "v1.6.0",
        probe_first_line: false,
        needs_vcrt: true,
    },
];

/// The pin that installs `tool` (`ffprobe` comes with `ffmpeg`'s archive).
pub fn pin_for(tool: &str) -> Option<&'static ToolPin> {
    let tool = tool.trim_end_matches(".exe");
    PINS.iter().find(|p| p.tool == tool || p.members.iter().any(|m| m.exe.trim_end_matches(".exe") == tool))
}

/// The pinned member that is installed as `tool` (`ffprobe`, `ffprobe.exe`).
pub fn member_for<'a>(pin: &'a ToolPin, tool: &str) -> Option<&'a PinMember> {
    let tool = tool.trim_end_matches(".exe");
    pin.members.iter().find(|m| m.exe.trim_end_matches(".exe") == tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_hash(s: &str) -> bool {
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn every_pin_is_complete_and_well_formed() {
        assert!(!PINS.is_empty());
        for p in PINS {
            assert!(p.url.starts_with("https://github.com/"), "{}", p.url);
            assert!(!p.url.contains('?'), "no query string in a pinned URL");
            assert!(p.size > 0, "{}", p.tool);
            if let Some(h) = p.archive_sha256 {
                assert!(is_hash(h), "{} archive hash", p.tool);
            }
            assert!(!p.members.is_empty());
            for m in p.members {
                assert!(is_hash(m.sha256), "{} {}", p.tool, m.exe);
                assert!(m.exe.ends_with(".exe"));
                assert!(m.entry_suffix.ends_with(m.exe));
            }
            // A real pin always proves the build runs and is the version pinned.
            assert!(p.members.iter().any(|m| m.exe == p.probe_exe));
            assert!(!p.probe_args.is_empty() && !p.probe_contains.is_empty());
            assert!(p.probe_contains.contains(p.version));
        }
    }

    #[test]
    fn ffprobe_comes_from_the_ffmpeg_pin_and_unknown_tools_have_none() {
        assert_eq!(pin_for("ffmpeg").unwrap().tool, "ffmpeg");
        assert_eq!(pin_for("ffprobe").unwrap().tool, "ffmpeg");
        assert_eq!(pin_for("bmxtranswrap").unwrap().tool, "bmxtranswrap");
        assert!(pin_for("yt-dlp").is_none());
        assert!(pin_for("deno").is_none());
    }
}

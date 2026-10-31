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

/// The CPU family the pinned tools are chosen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Arm64,
}

impl Arch {
    pub fn label(self) -> &'static str {
        match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "ARM64",
        }
    }
}

/// Maps the native machine type of the host (`IMAGE_FILE_MACHINE_*`).
/// `None` for anything we do not pin a build for.
pub fn arch_from_native_machine(machine: u16) -> Option<Arch> {
    match machine {
        0xAA64 => Some(Arch::Arm64),
        0x8664 => Some(Arch::X64),
        _ => None,
    }
}

#[cfg(windows)]
fn native_machine() -> Result<u16, String> {
    use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE;
    use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
    let mut process = IMAGE_FILE_MACHINE(0);
    let mut native = IMAGE_FILE_MACHINE(0);
    // SAFETY: both out-pointers are valid for the call; the pseudo-handle needs no closing.
    unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, Some(&mut native)) }.map_err(|e| e.to_string())?;
    Ok(native.0)
}

#[cfg(not(windows))]
fn native_machine() -> Result<u16, String> {
    Ok(0x8664)
}

/// The host's CPU family, decided once. The machine the OS runs on, not the one this
/// exe was built for: an x64 daemon emulated on an ARM64 host still gets ARM64 tools.
/// x64 whenever it cannot be determined.
pub fn host_arch() -> Arch {
    static ARCH: std::sync::OnceLock<Arch> = std::sync::OnceLock::new();
    *ARCH.get_or_init(|| match native_machine() {
        Ok(m) => arch_from_native_machine(m).unwrap_or_else(|| {
            tracing::warn!("host architecture {m:#06x} is not one we pin tools for; using the x64 builds");
            Arch::X64
        }),
        Err(e) => {
            tracing::warn!("host architecture could not be determined ({e}); using the x64 builds");
            Arch::X64
        }
    })
}

/// bmxtranswrap has no ARM64 build: the same x64 pin on every host.
const BMX_PIN: ToolPin = ToolPin {
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
};

/// Native ARM64 ffmpeg/ffprobe (BtbN, GPL build), pinned for ARM64 hosts: the x64 build
/// runs under emulation there and has hung. BtbN keeps each month's last build for two years.
pub const ARM64_PINS: &[ToolPin] = &[
    ToolPin {
        tool: "ffmpeg",
        version: "9.0.2-39-g7adfbe40dc",
        url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-10-31-12-59/ffmpeg-n9.0.2-39-g7adfbe40dc-winarm64-gpl-9.0.zip",
        size: 136_603_040,
        archive_sha256: Some("8b3c394762f26716ccef03ac1b7ac76430759080e1255aa9bc01d74db590bcb5"),
        members: &[
            PinMember {
                exe: "ffmpeg.exe",
                entry_suffix: "bin/ffmpeg.exe",
                sha256: "e2248aa5ecd155814e951fd0606df71589699a2fd4db14a9323a0b400a6c9b06",
            },
            PinMember {
                exe: "ffprobe.exe",
                entry_suffix: "bin/ffprobe.exe",
                sha256: "e947394db911aff33c085d211ccc723d2bd9e1a08dd8c16fb6f295546a1c5d14",
            },
        ],
        probe_exe: "ffmpeg.exe",
        probe_args: &["-version"],
        probe_contains: "n9.0.2-39-g7adfbe40dc",
        probe_first_line: true,
        needs_vcrt: false,
    },
    BMX_PIN,
];

pub const X64_PINS: &[ToolPin] = &[
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
    BMX_PIN,
];

pub fn pins_for(arch: Arch) -> &'static [ToolPin] {
    match arch {
        Arch::X64 => X64_PINS,
        Arch::Arm64 => ARM64_PINS,
    }
}

/// The pins for this host (see `host_arch`).
pub fn pins() -> &'static [ToolPin] {
    pins_for(host_arch())
}

/// The pins of the other architecture, to recognise a build of the wrong one.
pub fn other_pins() -> &'static [ToolPin] {
    pins_for(match host_arch() {
        Arch::X64 => Arch::Arm64,
        Arch::Arm64 => Arch::X64,
    })
}

/// The pin that installs `tool` (`ffprobe` comes with `ffmpeg`'s archive).
pub fn pin_for(tool: &str) -> Option<&'static ToolPin> {
    let tool = tool.trim_end_matches(".exe");
    pins().iter().find(|p| p.tool == tool || p.members.iter().any(|m| m.exe.trim_end_matches(".exe") == tool))
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
        for list in [X64_PINS, ARM64_PINS] {
            check_well_formed(list);
        }
    }

    fn check_well_formed(list: &[ToolPin]) {
        assert!(!list.is_empty());
        let names: Vec<&str> = list.iter().map(|p| p.tool).collect();
        assert_eq!(names, vec!["ffmpeg", "bmxtranswrap"]);
        let ff = list.iter().find(|p| p.tool == "ffmpeg").unwrap();
        let exes: Vec<&str> = ff.members.iter().map(|m| m.exe).collect();
        assert_eq!(exes, vec!["ffmpeg.exe", "ffprobe.exe"]);
        for p in list {
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

    #[test]
    fn the_architectures_differ_only_in_ffmpeg() {
        let find = |l: &'static [ToolPin], t: &str| l.iter().find(|p| p.tool == t).unwrap();
        let (x, a) = (find(X64_PINS, "ffmpeg"), find(ARM64_PINS, "ffmpeg"));
        assert_ne!(x.url, a.url);
        assert_ne!(x.version, a.version);
        for (mx, ma) in x.members.iter().zip(a.members) {
            assert_eq!(mx.exe, ma.exe);
            assert_ne!(mx.sha256, ma.sha256, "{}", mx.exe);
        }
        let (bx, ba) = (find(X64_PINS, "bmxtranswrap"), find(ARM64_PINS, "bmxtranswrap"));
        assert_eq!(format!("{bx:?}"), format!("{ba:?}"));
        assert_eq!(pins_for(Arch::X64)[0].url, X64_PINS[0].url);
        assert_eq!(pins_for(Arch::Arm64)[0].url, ARM64_PINS[0].url);
    }

    #[test]
    fn native_machine_types_map_to_an_architecture() {
        assert_eq!(arch_from_native_machine(0xAA64), Some(Arch::Arm64));
        assert_eq!(arch_from_native_machine(0x8664), Some(Arch::X64));
        assert_eq!(arch_from_native_machine(0x014c), None);
        assert_eq!(arch_from_native_machine(0), None);
        assert_eq!(arch_from_native_machine(0xFFFF), None);
    }

    #[test]
    fn host_arch_is_decided_without_panicking_and_is_stable() {
        let a = host_arch();
        println!("host_arch = {a:?}");
        assert_eq!(a, host_arch());
        assert_eq!(pins()[0].url, pins_for(a)[0].url);
        assert_ne!(other_pins()[0].url, pins()[0].url);
    }
}

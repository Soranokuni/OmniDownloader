//! Setup's command line, parsed by hand: switches are case-insensitive and may
//! be written `/x`, `-x` or `--x`.

use std::path::PathBuf;

use omni_service::DEFAULT_SERVICE_ACCOUNT;

/// A wrong command line. The text never repeats a plain argument (it might be
/// a password typed in the wrong place).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct UsageError(pub String);

/// What an elevated run does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Install or upgrade from the payload.
    Install { dir: PathBuf, account: Option<String>, no_firewall: bool, answer: Option<PathBuf> },
    /// Register the installed service under another account; `None` = ask at the console.
    ChangeAccount { account: Option<String>, no_firewall: bool },
    /// Remove the service and the program files; the data too with `remove_data`.
    Uninstall { dir: Option<PathBuf>, remove_data: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    /// Build a Setup from this one's stub and a release folder.
    Pack { folder: PathBuf, out: PathBuf },
    /// Check a packed Setup's payload (hidden; for the packaging script).
    Verify { file: PathBuf },
    Run { mode: Mode, quiet: bool, elevated_child: bool },
}

pub const USAGE: &str = r#"OmniIngest Setup

  OmniIngest-Setup.exe [/dir <folder>] [/account <name>] [/no-firewall] [/answer <file>] [/quiet]
      Install OmniIngest, or upgrade the installation that is there.
      /dir       the install folder, an absolute path (default C:\OmniIngest)
      /account   the service account (default: the virtual account NT SERVICE\OmniIngestService)
      /answer    settings exported from the admin page, for a new installation
      /quiet     ask nothing; for deployment tools running as administrator

  OmniIngest-Setup.exe /change-account [/account <name>] [/quiet]
      Run the installed service under another account ("default" = the virtual account).
      Without /account Setup asks for it; with /quiet, /account is required.

  OmniIngest-Setup.exe /uninstall [/remove-data] [/dir <folder>] [/quiet]
      Remove the service and the program files. /remove-data also removes the
      configuration, database, logs, work files and archive inside the install
      folder. The watchfolder and the videos in it are never removed.

  OmniIngest-Setup.exe --pack <release folder> --out <file>
      Build a Setup that carries the release folder.

  A password is never given on the command line: Setup asks for it at the prompt.

Exit codes: 0 done, 1 failed, 2 wrong command line, 3 needs administrator rights,
4 damaged Setup file, 5 refused (nothing changed), 6 cancelled, 7 upgrade rolled back.
"#;

const NO_PASSWORD: &str =
    "There is no password switch: a password is only ever typed at Setup's prompt, never given on the command line.";

fn usage(text: impl Into<String>) -> UsageError {
    UsageError(text.into())
}

/// The switch name of `arg` (`/Dir`, `-dir`, `--dir` → `dir`), or `None` for a plain argument.
fn switch_name(arg: &str) -> Option<String> {
    let name = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')).or_else(|| arg.strip_prefix('/'))?;
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

/// The arguments after the program name.
pub fn parse_args(args: &[String]) -> Result<Command, UsageError> {
    const WITH_VALUE: &[&str] = &["dir", "account", "answer", "pack", "out", "verify"];
    const FLAGS: &[&str] = &["no-firewall", "quiet", "change-account", "uninstall", "remove-data", "elevated-child"];

    let mut seen: Vec<(String, Option<String>)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let Some(name) = switch_name(&args[i]) else {
            return Err(usage(format!(
                "Argument {} is not a switch. Setup takes only switches; see OmniIngest-Setup.exe /?.",
                i + 1
            )));
        };
        if name == "?" || name == "help" || name == "h" {
            return Ok(Command::Help);
        }
        if name.contains("pass") || name.contains("pwd") {
            return Err(usage(NO_PASSWORD));
        }
        let value = if WITH_VALUE.contains(&name.as_str()) {
            match args.get(i + 1) {
                Some(v) if switch_name(v).is_none() && !v.trim().is_empty() => {
                    i += 1;
                    Some(v.clone())
                }
                _ => return Err(usage(format!("/{name} needs a value."))),
            }
        } else if FLAGS.contains(&name.as_str()) {
            None
        } else {
            return Err(usage(format!("Argument {} is not a known switch.", i + 1)));
        };
        if seen.iter().any(|(n, _)| *n == name) {
            return Err(usage(format!("/{name} is given twice.")));
        }
        seen.push((name, value));
        i += 1;
    }

    let has = |n: &str| seen.iter().any(|(s, _)| s == n);
    let value = |n: &str| seen.iter().find(|(s, _)| s == n).and_then(|(_, v)| v.clone());

    if has("pack") || has("out") {
        if !(has("pack") && has("out")) {
            return Err(usage("--pack needs --out <file>, and --out is only for --pack."));
        }
        if seen.len() != 2 {
            return Err(usage("--pack cannot be combined with any other switch."));
        }
        return Ok(Command::Pack { folder: value("pack").unwrap_or_default().into(), out: value("out").unwrap_or_default().into() });
    }
    if has("verify") {
        if seen.len() != 1 {
            return Err(usage("--verify cannot be combined with any other switch."));
        }
        return Ok(Command::Verify { file: value("verify").unwrap_or_default().into() });
    }

    let quiet = has("quiet");
    let elevated_child = has("elevated-child");
    let account = value("account").map(|a| if a.trim().eq_ignore_ascii_case("default") { DEFAULT_SERVICE_ACCOUNT.to_string() } else { a });
    let dir = match value("dir") {
        Some(d) => {
            let p = PathBuf::from(&d);
            if !p.is_absolute() {
                return Err(usage("/dir needs an absolute path such as C:\\OmniIngest."));
            }
            Some(p)
        }
        None => None,
    };
    if has("remove-data") && !has("uninstall") {
        return Err(usage("/remove-data is only for /uninstall."));
    }
    let mode = match (has("change-account"), has("uninstall")) {
        (true, true) => return Err(usage("/change-account and /uninstall cannot be combined.")),
        (true, false) => {
            if has("answer") {
                return Err(usage("/answer is only for an install."));
            }
            if dir.is_some() {
                return Err(usage("/change-account takes no /dir: the folder is the installed service's."));
            }
            if account.is_none() && quiet {
                return Err(usage("/change-account /quiet needs /account <name> (\"default\" for the virtual account)."));
            }
            Mode::ChangeAccount { account, no_firewall: has("no-firewall") }
        }
        (false, true) => {
            if has("answer") {
                return Err(usage("/answer is only for an install."));
            }
            for n in ["account", "no-firewall"] {
                if has(n) {
                    return Err(usage(format!("/{n} is not for /uninstall.")));
                }
            }
            Mode::Uninstall { dir, remove_data: has("remove-data") }
        }
        (false, false) => Mode::Install {
            dir: dir.unwrap_or_else(|| PathBuf::from(omni_cli::install::DEFAULT_DIR)),
            account,
            no_firewall: has("no-firewall"),
            answer: value("answer").map(PathBuf::from),
        },
    };
    Ok(Command::Run { mode, quiet, elevated_child })
}

/// The arguments of the elevated relaunch: `args` with `/answer`'s file made
/// absolute against `cwd` (the elevated child starts elsewhere, usually in
/// System32), then `/elevated-child`.
pub fn relaunch_args(args: &[String], cwd: &std::path::Path) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 1);
    let mut answer_next = false;
    for a in args {
        if answer_next && switch_name(a).is_none() {
            let p = std::path::Path::new(a);
            out.push(if p.is_absolute() { a.clone() } else { cwd.join(p).display().to_string() });
        } else {
            out.push(a.clone());
        }
        answer_next = switch_name(a).as_deref() == Some("answer");
    }
    out.push("/elevated-child".into());
    out
}

/// `args` as one Windows command line (the rules `CommandLineToArgvW` and the
/// C runtime read back): for the elevated relaunch.
pub fn command_line(args: &[String]) -> String {
    let mut out = String::new();
    for (n, a) in args.iter().enumerate() {
        if n > 0 {
            out.push(' ');
        }
        if !a.is_empty() && !a.contains([' ', '\t', '\n', '\x0b', '"']) {
            out.push_str(a);
            continue;
        }
        out.push('"');
        let mut backslashes = 0;
        for c in a.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                }
                _ => {
                    out.push_str(&"\\".repeat(backslashes));
                    out.push(c);
                    backslashes = 0;
                }
            }
        }
        out.push_str(&"\\".repeat(backslashes * 2));
        out.push('"');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(line: &[&str]) -> Result<Command, UsageError> {
        parse_args(&line.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn run(mode: Mode, quiet: bool) -> Command {
        Command::Run { mode, quiet, elevated_child: false }
    }

    fn install() -> Mode {
        Mode::Install { dir: PathBuf::from(omni_cli::install::DEFAULT_DIR), account: None, no_firewall: false, answer: None }
    }

    #[test]
    fn every_mode_parses_in_every_spelling() {
        assert_eq!(parse(&[]).unwrap(), run(install(), false));
        for help in ["/?", "/help", "--help", "-?", "/HELP"] {
            assert_eq!(parse(&[help]).unwrap(), Command::Help, "{help}");
        }
        for (dir, account, nf, quiet) in [("/dir", "/account", "/no-firewall", "/quiet"), ("--DIR", "-Account", "--No-Firewall", "-QUIET")] {
            let c = parse(&[dir, r"D:\Omni Ingest", account, r"DOMAIN\svc_test", nf, quiet, "/answer", r"C:\a.json"]).unwrap();
            assert_eq!(
                c,
                run(
                    Mode::Install {
                        dir: PathBuf::from(r"D:\Omni Ingest"),
                        account: Some(r"DOMAIN\svc_test".into()),
                        no_firewall: true,
                        answer: Some(PathBuf::from(r"C:\a.json")),
                    },
                    true
                )
            );
        }
        assert_eq!(
            parse(&["/change-account", "/account", "default"]).unwrap(),
            run(Mode::ChangeAccount { account: Some(DEFAULT_SERVICE_ACCOUNT.into()), no_firewall: false }, false)
        );
        assert_eq!(
            parse(&["/Change-Account", "/account", r"DOMAIN\svc_test", "/quiet"]).unwrap(),
            run(Mode::ChangeAccount { account: Some(r"DOMAIN\svc_test".into()), no_firewall: false }, true)
        );
        // Apps & Features' Modify button: interactive, the account is asked for.
        assert_eq!(parse(&["/change-account"]).unwrap(), run(Mode::ChangeAccount { account: None, no_firewall: false }, false));
        assert_eq!(parse(&["/uninstall"]).unwrap(), run(Mode::Uninstall { dir: None, remove_data: false }, false));
        assert_eq!(
            parse(&["/uninstall", "/remove-data", "/quiet", "/dir", r"E:\Omni"]).unwrap(),
            run(Mode::Uninstall { dir: Some(PathBuf::from(r"E:\Omni")), remove_data: true }, true)
        );
        assert_eq!(
            parse(&["--pack", r"C:\release", "--out", r"C:\out\Setup.exe"]).unwrap(),
            Command::Pack { folder: r"C:\release".into(), out: r"C:\out\Setup.exe".into() }
        );
        assert_eq!(parse(&["--verify", "x.exe"]).unwrap(), Command::Verify { file: "x.exe".into() });
        assert_eq!(
            parse(&["/uninstall", "/elevated-child"]).unwrap(),
            Command::Run { mode: Mode::Uninstall { dir: None, remove_data: false }, quiet: false, elevated_child: true }
        );
    }

    #[test]
    fn every_usage_error_is_refused() {
        let cases: &[&[&str]] = &[
            &["/frobnicate"],
            &["install"],
            &["/dir"],
            &["/dir", "/quiet"],
            &["/account"],
            &["/answer"],
            &["/dir", "relative\\path"],
            &["/remove-data"],
            &["/remove-data", "/quiet"],
            &["/answer", "a.json", "/uninstall"],
            &["/answer", "a.json", "/change-account", "/account", "x"],
            &["/change-account", "/quiet"],
            &["/change-account", "/uninstall"],
            &["/change-account", "/account", "x", "/dir", r"C:\x"],
            &["/uninstall", "/account", "x"],
            &["/uninstall", "/no-firewall"],
            &["--pack", r"C:\release"],
            &["--out", r"C:\x.exe"],
            &["--pack", r"C:\release", "--out", r"C:\x.exe", "/quiet"],
            &["--pack", r"C:\release", "--out", r"C:\x.exe", "/uninstall"],
            &["--verify", "x.exe", "/quiet"],
            &["/quiet", "/quiet"],
            &["/password", "x"],
            &["--PASSWORD=x"],
            &["/pwd", "x"],
        ];
        for line in cases {
            assert!(parse(line).is_err(), "{line:?} was accepted");
        }
    }

    #[test]
    fn a_password_switch_says_passwords_are_typed_at_the_prompt_and_no_argument_is_repeated() {
        for line in [&["/password", "Secret-1"][..], &["/Pass", "Secret-1"], &["--account-password=Secret-1"]] {
            let e = parse(line).unwrap_err();
            assert_eq!(e.0, NO_PASSWORD);
            assert!(!e.0.contains("Secret-1"));
        }
        // A stray value (a password in the wrong place) is never echoed.
        let e = parse(&["/account", r"DOMAIN\svc", "Secret-1"]).unwrap_err();
        assert!(!e.0.contains("Secret-1"), "{e}");
    }

    /// R9: a stray word may be a password typed in the wrong place; no usage
    /// error repeats any argument's text.
    #[test]
    fn no_usage_error_repeats_an_argument() {
        let cases: &[&[&str]] = &[
            &["/account", r"DOMAIN\svc", "-Hunter2!"],
            &["/account", r"DOMAIN\svc", "Hunter2!"],
            &["/Hunter2!"],
            &["--Hunter2!=x"],
            &["/dir", r"relHunter2!\x"],
            &["/uninstall", "/account", "Hunter2!"],
            &["--pass=Hunter2!"],
            &["/answer", "Hunter2!.json", "/uninstall"],
        ];
        for line in cases {
            let e = parse(line).unwrap_err();
            assert!(!e.0.contains("Hunter2"), "{line:?}: {e}");
        }
        assert_eq!(parse(&["/quiet", "/Hunter2!"]).unwrap_err().0, "Argument 2 is not a known switch.");
    }

    /// R10: the elevated child's working folder is not the operator's.
    #[test]
    fn the_relaunch_makes_the_answer_file_absolute_and_marks_the_child() {
        let cwd = Path::new(r"C:\Users\op\Downloads");
        let args: Vec<String> = ["/quiet", "/ANSWER", r"answers\omni.json", "/dir", r"D:\Omni"].map(String::from).to_vec();
        assert_eq!(
            relaunch_args(&args, cwd),
            ["/quiet", "/ANSWER", r"C:\Users\op\Downloads\answers\omni.json", "/dir", r"D:\Omni", "/elevated-child"]
        );
        let abs: Vec<String> = ["--answer", r"E:\a.json"].map(String::from).to_vec();
        assert_eq!(relaunch_args(&abs, cwd), ["--answer", r"E:\a.json", "/elevated-child"]);
    }

    #[test]
    fn the_relaunch_command_line_reads_back_as_the_same_arguments() {
        let args: Vec<String> =
            ["/dir", r"C:\Program Files\Omni Ingest\", "/account", r#"we"ird"#, "", r"C:\a\b", "/elevated-child"].map(String::from).to_vec();
        assert_eq!(
            command_line(&args),
            r#"/dir "C:\Program Files\Omni Ingest\\" /account "we\"ird" "" C:\a\b /elevated-child"#
        );
    }
}

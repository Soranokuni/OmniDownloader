//! The console around the `omni_deploy` library: parse, run, print, exit.

use std::io::BufRead;

use omni_deploy::args::{parse_args, Command, USAGE};
use omni_deploy::exit::{exit_code_for, ExitCode};
use omni_deploy::log::SetupLog;
use omni_deploy::run::{flush, pack_self, run, verify_file, Console};

fn read_line() -> String {
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line
}

/// Questions at this console; end of input is no answer.
struct StdConsole;

impl Console for StdConsole {
    fn confirm(&mut self, prompt: &str) -> bool {
        self.ask(prompt).is_some_and(|a| matches!(a.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
    }

    fn ask(&mut self, prompt: &str) -> Option<String> {
        print!("{prompt}");
        flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

fn main() {
    // First: every DLL loaded later (shell, COM, UAC) comes from System32, never
    // from the folder Setup was downloaded to.
    omni_deploy::elevate::restrict_dll_search();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("See OmniIngest-Setup.exe /? for the switches.");
            std::process::exit(ExitCode::Usage.code() as i32);
        }
    };
    let code = match command {
        Command::Help => {
            print!("{USAGE}");
            ExitCode::Ok.code()
        }
        Command::Pack { folder, out } => match pack_self(&folder, &out) {
            Ok(p) => {
                println!("{}: {} bytes, payload SHA-256 {}", out.display(), p.size, p.sha256_hex);
                ExitCode::Ok.code()
            }
            Err(e) => {
                eprintln!("{e:#}");
                exit_code_for(&e).code()
            }
        },
        Command::Verify { file } => match verify_file(&file) {
            Ok(span) => {
                println!("{}: payload verified ({} bytes after a {}-byte stub).", file.display(), span.zip_len, span.zip_start);
                ExitCode::Ok.code()
            }
            Err(e) => {
                eprintln!("{e:#}");
                exit_code_for(&e).code()
            }
        },
        Command::Run { mode, quiet, elevated_child } => {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Setup cannot start: {e}");
                    std::process::exit(ExitCode::Failed.code() as i32);
                }
            };
            let mut log = SetupLog::new(!quiet);
            let finished = runtime.block_on(run(&args, mode, quiet, elevated_child, &mut StdConsole, &mut log));
            if let Some(err) = &finished.error {
                eprintln!("{err}");
            }
            // The elevated child's own console closes when it exits: let the operator read it.
            if elevated_child && !quiet {
                print!("\nPress Enter to close.");
                flush();
                let _ = read_line();
            }
            finished.code
        }
    };
    std::process::exit(code as i32);
}

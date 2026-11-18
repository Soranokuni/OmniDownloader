//! The windowed program around the `omni_deploy` library: parse, then either
//! print (attached to the console it was started from) or show Setup's pages.
//! A double-click shows no console. `console.rs` and `dialogs.rs` are this
//! binary's own modules: the library never links the task dialog.
#![windows_subsystem = "windows"]

mod console;
mod dialogs;

use omni_deploy::args::{parse_args, Command, Face, USAGE};
use omni_deploy::backend::DemoBackend;
use omni_deploy::exit::{exit_code_for, ExitCode};
use omni_deploy::flow;
use omni_deploy::log::SetupLog;
use omni_deploy::run::{pack_self, run_pages, run_quiet, verify_file};
use omni_deploy::texts::Lang;

use dialogs::TaskDialogUi;

fn main() {
    // First: every DLL loaded later (shell, COM, UAC) comes from System32, never
    // from the folder Setup was downloaded to.
    omni_deploy::elevate::restrict_dll_search();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            let text = format!("{e}\nSee OmniIngest-Setup.exe /? for the switches.");
            if console::attach() {
                eprintln!("{text}");
            } else {
                // Started without any console (a shortcut with a wrong switch).
                dialogs::message_box(Lang::detect(), &text, true);
            }
            std::process::exit(ExitCode::Usage.code() as i32);
        }
    };
    // A printing command never shows a page; the pages never print.
    let printing = match command.face() {
        Face::Console => console::attach(),
        Face::Pages => {
            dialogs::prepare();
            false
        }
    };
    let code = match command {
        Command::Help => {
            if printing {
                print!("{USAGE}");
            } else {
                dialogs::message_box(Lang::detect(), USAGE, false);
            }
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
        Command::Run { mode, quiet: true, elevated_child, .. } => {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Setup cannot start: {e}");
                    std::process::exit(ExitCode::Failed.code() as i32);
                }
            };
            // As before: `/quiet` prints only its error; setup.log has every line.
            let mut log = SetupLog::new(false);
            let finished = runtime.block_on(run_quiet(mode, elevated_child, &mut log));
            if let Some(err) = &finished.error {
                eprintln!("{err}");
            }
            finished.code
        }
        Command::Run { mode, quiet: false, elevated_child, lang } => {
            let lang = lang.unwrap_or_else(Lang::detect);
            let mut new_ui = || -> Box<dyn omni_deploy::ui::Ui> { Box::new(TaskDialogUi::new(lang, false)) };
            let mut tell = |text: &str| dialogs::message_box(lang, text, false);
            run_pages(&args, mode, lang, elevated_child, &mut new_ui, &mut tell).code
        }
        Command::PagesDemo { scenario, auto, lang } => {
            // Never elevated, never touching the machine: the fake backend.
            let lang = lang.unwrap_or_else(Lang::detect);
            let mut ui = TaskDialogUi::new(lang, auto);
            let finished = flow::interactive(&mut ui, &mut DemoBackend::new(scenario), scenario.start());
            if finished.code == scenario.expected_code() {
                ExitCode::Ok.code()
            } else {
                ExitCode::Failed.code()
            }
        }
    };
    std::process::exit(code as i32);
}

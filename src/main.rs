#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
compile_error!("NUCtool's native application targets Windows. No hardware controls are implemented for other operating systems.");

mod backend;
mod ec;
mod fan_graph;
mod gpu;
mod journal;
mod machine;
mod model;
mod ui;

fn main() {
    if std::env::args().any(|arg| arg == "--diagnose") {
        // A release GUI has no inherited console; attach only for this explicit CLI mode.
        unsafe {
            windows_sys::Win32::System::Console::AttachConsole(u32::MAX);
        }
        match backend::diagnose() {
            Ok(report) => println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("serializable report")
            ),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    } else if let Err(e) = ui::run() {
        ui::error(&e);
    }
}

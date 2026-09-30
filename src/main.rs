#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod clean;
mod cli;
mod colors;
mod compress;
mod dedupe;
mod demo;
mod devjunk;
mod dupes;
mod export;
mod history;
mod icon;
mod monitor;
mod platform;
mod recommend;
mod risk;
#[cfg(windows)]
mod programs;
#[cfg(windows)]
mod reg;
#[cfg(windows)]
mod regclean;
mod relocate;
mod scan;
mod search;
#[cfg(any(windows, target_os = "linux"))]
mod sensors;
#[cfg(windows)]
mod startup;
#[cfg(windows)]
mod tray;
mod trashlog;
mod tree;
mod treemap;
mod util;
#[cfg(windows)]
mod winget;
#[cfg(windows)]
mod winsys;

use eframe::egui;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| cli::is_cli_command(a)) {
        platform::attach_parent_console();
        return cli::run(&args);
    }

    // Skip flags, including the `-psn_…` that older macOS passes to apps launched from Finder.
    let initial = args.iter().find(|a| !a.starts_with('-')).cloned();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Heft")
            .with_app_id("heft")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([820.0, 520.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Heft",
        options,
        Box::new(move |cc| Ok(Box::new(app::HeftApp::new(cc, initial)))),
    );
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("heft: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Window icon (Windows taskbar, Linux window managers; macOS uses the
/// bundle's .icns instead).
fn app_icon() -> egui::IconData {
    const SIZE: u32 = 128;
    egui::IconData { rgba: icon::rgba(SIZE), width: SIZE, height: SIZE }
}

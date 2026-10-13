//! Strela — fast file operations for Windows. One executable: the app (tray, hotkey, window), its own installer and
//! uninstaller (`--install` / `--uninstall`, or when named `Strela-Setup.exe`), and the optional indexer service
//! (`--service`).
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod backend;
mod commands;
mod gui;
mod service;
mod settings;
mod shell;
mod state;
mod winint;

use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

use commands::Command;
use gui::{App, Screen};
use settings::Settings;
use state::Shared;

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn install_panic_log() {
    std::panic::set_hook(Box::new(|info| {
        let dir = settings::dir();
        let _ = std::fs::create_dir_all(&dir);
        let line = format!("{}\n", info);
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")).and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())
        });
    }));
}

/// True for the first Strela process of this user; later ones forward their command and exit.
fn become_primary() -> bool {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let name: Vec<u16> = format!("Local\\Strela-Primary-{user}").encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: the handle is intentionally leaked: the mutex must live as long as the process.
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

fn viewport(screen: &Screen, hidden: bool) -> egui::ViewportBuilder {
    let (size, min, title, resizable) = match screen {
        Screen::Install(_) => ([600.0, 560.0], [600.0, 560.0], "Установка Strela", false),
        Screen::Uninstall(_) => ([560.0, 360.0], [560.0, 360.0], "Удаление Strela", false),
        _ => ([1180.0, 760.0], [900.0, 560.0], "Strela", true),
    };
    let mut vb = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size(min)
        .with_title(title)
        .with_app_id("Strela")
        .with_resizable(resizable)
        .with_transparent(true);
    if hidden {
        // eframe always shows the window once after the first frame; keep that moment off-screen and tiny. The
        // window is hidden right after, and gets its real size and position when it is first shown.
        vb = vb.with_position([-32000.0, -32000.0]).with_inner_size([8.0, 8.0]).with_min_inner_size([8.0, 8.0]);
    }
    if !resizable {
        vb = vb.with_maximize_button(false);
    }
    vb.with_icon(egui::IconData { rgba: gui::LOGO.to_vec(), width: 64, height: 64 })
}

fn run_gui(shared: Arc<Shared>, screen: Screen, hidden: bool) -> eframe::Result {
    let vb = viewport(&screen, hidden);
    let opts = eframe::NativeOptions { viewport: vb, ..Default::default() };
    let mut screen = Some(screen);
    eframe::run_native(
        "Strela",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(cc, shared, screen.take().unwrap_or(Screen::Main), hidden)))),
    )
}

fn main() {
    install_panic_log();
    let args: Vec<String> = std::env::args().collect();

    // ---- service entry points
    if has(&args, "--service") {
        let _ = service::run_dispatcher();
        return;
    }
    if has(&args, "--service-console") {
        service::run_console(&args);
        return;
    }
    if has(&args, "--install-service") {
        std::process::exit(if service::install_elevated().is_ok() { 0 } else { 2 });
    }
    if has(&args, "--uninstall-service") {
        std::process::exit(if service::uninstall_elevated().is_ok() { 0 } else { 2 });
    }

    // ---- installer / uninstaller
    if has(&args, "--uninstall") {
        if has(&args, "--quiet") {
            std::process::exit(if winint::uninstall().is_ok() { 0 } else { 1 });
        }
        let shared = Shared::new(Settings::default());
        let _ = run_gui(shared, Screen::Uninstall(Default::default()), false);
        return;
    }
    if has(&args, "--install") && has(&args, "--quiet") {
        // silent install: `Strela.exe --install --quiet [--no-menu] [--no-sendto] [--no-autostart] [--no-service] [--desktop]`
        let opts = winint::InstallOptions {
            desktop_shortcut: has(&args, "--desktop"),
            context_menu: !has(&args, "--no-menu"),
            send_to: !has(&args, "--no-sendto"),
            autostart: !has(&args, "--no-autostart"),
            service: !has(&args, "--no-service"),
        };
        match winint::install(opts) {
            Ok(r) => {
                if let Err(e) = r.service {
                    eprintln!("service: {e}");
                }
                std::process::exit(0)
            }
            Err(e) => {
                let _ = std::fs::write(std::env::temp_dir().join("Strela-install.log"), e);
                std::process::exit(1)
            }
        }
    }
    if winint::is_setup_launch() && !winint::running_installed() {
        let shared = Shared::new(Settings::default());
        let _ = run_gui(shared, Screen::Install(Default::default()), false);
        return;
    }

    // ---- the app
    let cmd = commands::parse_args(&args);
    if !become_primary() {
        // SAFETY: lets the running instance take the foreground for the window the command may open.
        unsafe { AllowSetForegroundWindow(u32::MAX) };
        let cmd = cmd.unwrap_or(Command { verb: "open".into(), paths: vec![], dest: None });
        let _ = commands::send_to_primary(&cmd, Duration::from_secs(8));
        return;
    }
    let settings = Settings::load();
    let shared = Shared::new(settings);
    commands::start_server(shared.clone());
    shell::start(shared.clone());
    if winint::running_installed() {
        // keep the "Send to" drive list in step with the drives that exist now
        std::thread::spawn(|| {
            if let Ok(exe) = std::env::current_exe() {
                let _ = winint::sync_sendto(&exe, winint::sendto_dir().is_some_and(|d| {
                    std::fs::read_dir(d).is_ok_and(|r| r.flatten().any(|e| e.file_name().to_string_lossy().starts_with("Strela - ")))
                }));
            }
        });
    }
    let needs_window = cmd.as_ref().is_none_or(|c| matches!(c.verb.as_str(), "open" | "spotlight" | "move-to" | "copy-to" | "fast-delete" | "recycle" | "find-here" | "largest-here"));
    let hidden = has(&args, "--hidden") || !needs_window;
    if let Some(c) = cmd {
        // the pipe server is already up: the command takes the same path as one from another process
        commands::submit_local(c);
    }
    if let Err(e) = run_gui(shared.clone(), Screen::Main, hidden) {
        eprintln!("{e}");
    }
    shared.quit();
}

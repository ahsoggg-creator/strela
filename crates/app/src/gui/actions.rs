//! Opening things, and the "Strela actions" menu shown for files in every list.

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui::Ui;
use windows_sys::Win32::UI::Shell::ShellExecuteW;

use crate::commands::{self, Command};
use crate::state::Shared;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens a file or folder with its default program (like double-clicking in Explorer).
pub fn open_path(path: &str) {
    let (verb, file) = (wide("open"), wide(path));
    // SAFETY: NUL-terminated strings; no window handle, no parameters.
    unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
}

/// Shows the file selected in an Explorer window.
pub fn reveal(path: &str) {
    use std::os::windows::process::CommandExt;
    let p = path.trim_end_matches('\\');
    if std::path::Path::new(p).is_dir() {
        open_path(p);
    } else {
        let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{p}\"")).spawn();
    }
}

/// Opens `path` at `line`. `editor` is a command template with `{path}` and `{line}`; empty uses the default
/// program (no line). Known editors are tried when the template is empty and one is installed.
pub fn open_at_line(editor: &str, path: &str, line: u64) {
    use std::os::windows::process::CommandExt;
    let template = if editor.trim().is_empty() { detect_editor() } else { Some(editor.to_string()) };
    match template {
        Some(t) => {
            let cmd = t.replace("{path}", path).replace("{line}", &line.to_string());
            // run through cmd so quoting in the template works as the user wrote it
            let _ = std::process::Command::new("cmd.exe").raw_arg(format!("/C \"{cmd}\"")).creation_flags(0x0800_0000).spawn();
        }
        None => open_path(path),
    }
}

/// VS Code or Notepad++ when installed (both can open at a line).
fn detect_editor() -> Option<String> {
    let on_path = |exe: &str| {
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).exists()))
    };
    if on_path("code.cmd") || on_path("code.exe") {
        return Some("code -g \"{path}:{line}\"".into());
    }
    for base in [std::env::var("ProgramFiles").ok(), std::env::var("ProgramFiles(x86)").ok()].into_iter().flatten() {
        let n = std::path::Path::new(&base).join("Notepad++").join("notepad++.exe");
        if n.exists() {
            return Some(format!("\"{}\" -n{{line}} \"{{path}}\"", n.display()));
        }
    }
    None
}

fn run(shared: &Arc<Shared>, verb: &str, paths: &[PathBuf]) {
    commands::execute(
        shared,
        Command { verb: verb.into(), paths: paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(), dest: None },
    );
}

/// Context-menu body for one or several paths. Returns true if the menu should close.
pub fn path_menu(ui: &mut Ui, shared: &Arc<Shared>, paths: &[PathBuf]) -> bool {
    let Some(first) = paths.first() else { return false };
    let first_s = first.to_string_lossy().into_owned();
    let single = paths.len() == 1;
    let is_dir = single && first.is_dir();
    let mut close = false;
    let mut item = |ui: &mut Ui, text: &str| -> bool {
        let c = ui.button(text).clicked();
        close |= c;
        c
    };
    if single {
        if item(ui, "Открыть") {
            open_path(&first_s);
        }
        if item(ui, "Показать в папке") {
            reveal(&first_s);
        }
        if item(ui, "Копировать путь") {
            ui.ctx().copy_text(first_s.clone());
        }
        ui.separator();
    }
    if item(ui, "Копировать через Strela") {
        run(shared, "copy", paths);
    }
    if item(ui, "Вырезать через Strela") {
        run(shared, "cut", paths);
    }
    if is_dir && item(ui, "Вставить в эту папку") {
        run(shared, "paste", paths);
    }
    if item(ui, "Переместить на диск…") {
        run(shared, "move-to", paths);
    }
    ui.separator();
    if item(ui, "Быстро удалить…") {
        run(shared, "fast-delete", paths);
    }
    if item(ui, "Удалить в корзину…") {
        run(shared, "recycle", paths);
    }
    if is_dir {
        ui.separator();
        if item(ui, "Найти здесь (по имени)") {
            run(shared, "find-here", paths);
        }
        if item(ui, "Найти здесь (по содержимому)") {
            run(shared, "find-content-here", paths);
        }
        if item(ui, "Самые большие файлы здесь") {
            run(shared, "largest-here", paths);
        }
    }
    close
}

#[cfg(test)]
mod tests {
    #[test]
    fn editor_template_substitution() {
        let t = "code -g \"{path}:{line}\"";
        assert_eq!(t.replace("{path}", r"C:\a b\c.rs").replace("{line}", "42"), "code -g \"C:\\a b\\c.rs:42\"");
    }
}

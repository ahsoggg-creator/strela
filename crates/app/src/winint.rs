//! Windows integration. Everything except the optional indexer service is per-user (HKCU, the user's profile), so
//! installing needs no administrator rights: install folder, Start menu / desktop shortcuts, the Explorer context
//! menu, "Send to" entries, autostart and the "Apps & features" entry.

use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile};
use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Programs, FOLDERID_SendTo, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellLink};
use windows::core::{Interface, HSTRING};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

pub const APP: &str = "Strela";
pub const REPO_URL: &str = "https://github.com/ahsoggg-creator/strela";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Strela";
const APP_PATHS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\App Paths\Strela.exe";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const SENDTO_PREFIX: &str = "Strela - ";

#[derive(Clone, Copy, Debug)]
pub struct InstallOptions {
    pub desktop_shortcut: bool,
    pub context_menu: bool,
    pub send_to: bool,
    pub autostart: bool,
    pub service: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        InstallOptions { desktop_shortcut: false, context_menu: true, send_to: true, autostart: true, service: true }
    }
}

/// `%LOCALAPPDATA%\Programs\Strela`.
pub fn install_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Programs").join(APP))
}

pub fn installed_exe() -> Option<PathBuf> {
    install_dir().map(|d| d.join("Strela.exe"))
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().to_lowercase().replace('/', "\\")
}

/// The running executable is the installed copy.
pub fn running_installed() -> bool {
    match (std::env::current_exe(), installed_exe()) {
        (Ok(cur), Some(inst)) => norm(&cur) == norm(&inst),
        _ => false,
    }
}

/// Started as the installer: named `*Setup*.exe` or run with `--install`.
pub fn is_setup_launch() -> bool {
    std::env::args().any(|a| a == "--install")
        || std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
            .is_some_and(|s| s.contains("setup"))
}

// ---------------------------------------------------------------------------------------------------------
// Registry

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key came from RegCreateKeyExW / RegOpenKeyExW and is closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

fn create_key(path: &str) -> Result<Key, String> {
    let p = wide(path);
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: `p` is NUL-terminated; the out-pointer references a live local.
    let rc = unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE | KEY_READ, std::ptr::null(), &mut h, std::ptr::null_mut())
    };
    if rc == ERROR_SUCCESS { Ok(Key(h)) } else { Err(format!("реестр: не удалось создать {path} (код {rc})")) }
}

fn open_key(path: &str, write: bool) -> Option<Key> {
    let p = wide(path);
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: as above.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, if write { KEY_WRITE | KEY_READ } else { KEY_READ }, &mut h) };
    (rc == ERROR_SUCCESS).then_some(Key(h))
}

fn set_str(key: &Key, name: Option<&str>, value: &str) -> Result<(), String> {
    let n = name.map(wide);
    let v = wide(value);
    // SAFETY: `v` is NUL-terminated and its byte length is passed.
    let rc = unsafe {
        RegSetValueExW(key.0, n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()), 0, REG_SZ, v.as_ptr() as *const u8, (v.len() * 2) as u32)
    };
    if rc == ERROR_SUCCESS { Ok(()) } else { Err(format!("реестр: запись значения не удалась (код {rc})")) }
}

fn set_dword(key: &Key, name: &str, value: u32) -> Result<(), String> {
    let n = wide(name);
    // SAFETY: the data pointer references a live u32 of the stated size.
    let rc = unsafe { RegSetValueExW(key.0, n.as_ptr(), 0, REG_DWORD, &value as *const u32 as *const u8, 4) };
    if rc == ERROR_SUCCESS { Ok(()) } else { Err(format!("реестр: запись значения не удалась (код {rc})")) }
}

pub fn key_exists(path: &str) -> bool {
    open_key(path, false).is_some()
}

#[allow(dead_code)] // used by the registry round-trip tests
pub fn get_str(path: &str, name: &str) -> Option<String> {
    let k = open_key(path, false)?;
    let n = wide(name);
    let mut len = 0u32;
    // SAFETY: first call only asks for the size.
    let rc = unsafe { RegQueryValueExW(k.0, n.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), &mut len) };
    if rc != ERROR_SUCCESS || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    // SAFETY: the buffer has `len` bytes.
    let rc = unsafe { RegQueryValueExW(k.0, n.as_ptr(), std::ptr::null(), std::ptr::null_mut(), buf.as_mut_ptr() as *mut u8, &mut len) };
    (rc == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string())
}

fn delete_tree(path: &str) {
    let p = wide(path);
    // SAFETY: deleting a missing key just returns an error code.
    unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, p.as_ptr()) };
}

fn delete_value(path: &str, name: &str) {
    if let Some(k) = open_key(path, true) {
        let n = wide(name);
        // SAFETY: NUL-terminated name.
        let rc = unsafe { RegDeleteValueW(k.0, n.as_ptr()) };
        debug_assert!(rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND);
    }
}

fn notify_shell() {
    // SAFETY: SHCNE_ASSOCCHANGED takes no items.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
}

// ---------------------------------------------------------------------------------------------------------
// Context menu

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    File,
    Folder,
    Background,
    Drive,
}

pub const MENU_TARGETS: [(&str, &str, Target); 4] = [
    (r"Software\Classes\*\shell\Strela", "%1", Target::File),
    (r"Software\Classes\Directory\shell\Strela", "%1", Target::Folder),
    (r"Software\Classes\Directory\Background\shell\Strela", "%V", Target::Background),
    (r"Software\Classes\Drive\shell\Strela", "%1", Target::Drive),
];

/// `(sub-key order name, verb, label)` for each target. The menu text is what Explorer shows under "Strela ▸".
pub fn menu_items(t: Target) -> Vec<(&'static str, &'static str, &'static str)> {
    const COPY: (&str, &str, &str) = ("10copy", "copy", "Копировать через Strela");
    const CUT: (&str, &str, &str) = ("20cut", "cut", "Вырезать через Strela");
    const PASTE: (&str, &str, &str) = ("30paste", "paste", "Вставить через Strela");
    const MOVE: (&str, &str, &str) = ("40moveto", "move-to", "Переместить на диск…");
    const DEL: (&str, &str, &str) = ("50fastdelete", "fast-delete", "Быстро удалить");
    const BIN: (&str, &str, &str) = ("60recycle", "recycle", "Удалить в корзину");
    const FIND: (&str, &str, &str) = ("70find", "find-here", "Найти здесь (по имени)");
    const FIND_TEXT: (&str, &str, &str) = ("75findtext", "find-content-here", "Найти здесь (по содержимому)");
    const BIG: (&str, &str, &str) = ("80largest", "largest-here", "Самые большие файлы здесь");
    match t {
        Target::File => vec![COPY, CUT, MOVE, DEL, BIN],
        Target::Folder => vec![COPY, CUT, PASTE, MOVE, DEL, BIN, FIND, FIND_TEXT, BIG],
        Target::Background => vec![PASTE, FIND, FIND_TEXT, BIG],
        Target::Drive => vec![PASTE, FIND, FIND_TEXT, BIG],
    }
}

pub fn context_menu_enabled() -> bool {
    key_exists(MENU_TARGETS[1].0)
}

/// Registers "Strela ▸" for files, folders, folder backgrounds and drives. On Windows 11 it appears in the classic
/// menu ("Show more options", Shift+F10).
pub fn set_context_menu(enable: bool, exe: &Path) -> Result<(), String> {
    for (base, arg, target) in MENU_TARGETS {
        if !enable {
            delete_tree(base);
            continue;
        }
        let k = create_key(base)?;
        set_str(&k, Some("MUIVerb"), "Strela")?;
        set_str(&k, Some("Icon"), &format!("\"{}\",0", exe.display()))?;
        set_str(&k, Some("SubCommands"), "")?;
        set_str(&k, Some("MultiSelectModel"), "Player")?;
        for (order, verb, label) in menu_items(target) {
            let item = create_key(&format!(r"{base}\shell\{order}"))?;
            set_str(&item, Some("MUIVerb"), label)?;
            set_str(&item, Some("Icon"), &format!("\"{}\",0", exe.display()))?;
            let cmd = create_key(&format!(r"{base}\shell\{order}\command"))?;
            set_str(&cmd, None, &format!("\"{}\" --cmd {verb} -- \"{arg}\"", exe.display()))?;
        }
    }
    notify_shell();
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Autostart

pub fn set_autostart(on: bool) -> Result<(), String> {
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let target = installed_exe().filter(|p| p.exists()).unwrap_or(exe);
        let k = create_key(RUN_KEY)?;
        set_str(&k, Some(APP), &format!("\"{}\" --hidden", target.display()))
    } else {
        delete_value(RUN_KEY, APP);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------
// Shortcuts (COM)

struct Com;

impl Com {
    fn init() -> Com {
        // SAFETY: balanced by Drop; S_FALSE (already initialised) is fine.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Com
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: matches CoInitializeEx in `init`.
        unsafe { CoUninitialize() };
    }
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    // SAFETY: the returned buffer is freed with CoTaskMemFree after copying.
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn make_shortcut(lnk: &Path, target: &Path, args: &str, desc: &str) -> Result<(), String> {
    let _com = Com::init();
    // SAFETY: standard IShellLink creation on an initialised apartment; all strings outlive the calls.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        link.SetPath(&HSTRING::from(target.to_string_lossy().as_ref())).map_err(|e| e.to_string())?;
        link.SetArguments(&HSTRING::from(args)).map_err(|e| e.to_string())?;
        link.SetDescription(&HSTRING::from(desc)).map_err(|e| e.to_string())?;
        link.SetIconLocation(&HSTRING::from(target.to_string_lossy().as_ref()), 0).map_err(|e| e.to_string())?;
        if let Some(dir) = target.parent() {
            let _ = link.SetWorkingDirectory(&HSTRING::from(dir.to_string_lossy().as_ref()));
        }
        let file: IPersistFile = link.cast().map_err(|e| e.to_string())?;
        if let Some(parent) = lnk.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        file.Save(&HSTRING::from(lnk.to_string_lossy().as_ref()), true).map_err(|e| e.to_string())
    }
}

fn start_menu_lnk() -> Option<PathBuf> {
    known_folder(&FOLDERID_Programs).map(|d| d.join("Strela.lnk"))
}

fn desktop_lnk() -> Option<PathBuf> {
    known_folder(&FOLDERID_Desktop).map(|d| d.join("Strela.lnk"))
}

pub fn sendto_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_SendTo)
}

fn drive_label(d: char) -> String {
    d.to_string()
}

/// One "copy to" and one "move to" entry per local drive in the Explorer "Send to" menu.
pub fn sync_sendto(exe: &Path, enable: bool) -> Result<(), String> {
    let Some(dir) = sendto_dir() else { return Err("не найдена папка SendTo".into()) };
    let _ = std::fs::create_dir_all(&dir);
    let wanted: Vec<(String, char, &str)> = if enable {
        strela_core::winapi::drive_letters(true)
            .into_iter()
            .flat_map(|d| {
                [
                    (format!("{SENDTO_PREFIX}копировать на диск {}.lnk", drive_label(d)), d, "send-copy"),
                    (format!("{SENDTO_PREFIX}переместить на диск {}.lnk", drive_label(d)), d, "send-move"),
                ]
            })
            .collect()
    } else {
        Vec::new()
    };
    // remove our stale entries
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(SENDTO_PREFIX) && name.ends_with(".lnk") && !wanted.iter().any(|w| w.0 == name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    for (name, d, verb) in wanted {
        let path = dir.join(&name);
        let desc = if verb == "send-copy" { "Копировать через Strela" } else { "Переместить через Strela" };
        make_shortcut(&path, exe, &format!("--cmd {verb} --dest {d}:\\ --"), desc)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Elevation

/// Runs `exe args` elevated (one UAC prompt) and waits. `Err` carries a user-readable reason; declining the prompt is
/// reported as "cancelled".
pub fn run_elevated(exe: &Path, args: &str) -> Result<u32, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(args);
    // SAFETY: SHELLEXECUTEINFOW is plain-old-data; all strings are NUL-terminated and outlive the call.
    unsafe {
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ptr();
        sei.nShow = 0;
        if ShellExecuteExW(&mut sei) == 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(1223) { "запрос прав администратора отклонён".into() } else { e.to_string() });
        }
        if sei.hProcess.is_null() {
            return Err("процесс не запущен".into());
        }
        let w = WaitForSingleObject(sei.hProcess, INFINITE);
        let mut code = 1u32;
        if w == WAIT_OBJECT_0 {
            GetExitCodeProcess(sei.hProcess, &mut code);
        }
        CloseHandle(sei.hProcess);
        Ok(code)
    }
}

// ---------------------------------------------------------------------------------------------------------
// Install / uninstall

/// Asks a running Strela to quit and waits until it is really gone (it saves the index and the queue first).
pub fn stop_running_instance() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::OpenMutexW;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let _ = crate::commands::send_to_primary(
        &crate::commands::Command { verb: "quit".into(), paths: vec![], dest: None },
        std::time::Duration::from_millis(0),
    );
    let user = std::env::var("USERNAME").unwrap_or_default();
    let name = wide(&format!(r"Local\Strela-Primary-{user}"));
    for _ in 0..300 {
        // SAFETY: opening a named mutex for SYNCHRONIZE only; the handle is closed at once.
        let h = unsafe { OpenMutexW(SYNCHRONIZE, 0, name.as_ptr()) };
        if h.is_null() {
            break;
        }
        // SAFETY: valid handle from OpenMutexW.
        unsafe { CloseHandle(h) };
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub struct InstallResult {
    pub exe: PathBuf,
    /// `Ok(true)` installed, `Ok(false)` not requested, `Err` reason it was skipped.
    pub service: Result<bool, String>,
}

/// Copies the running executable into `%LOCALAPPDATA%\Programs\Strela` and registers everything.
pub fn install(opts: InstallOptions) -> Result<InstallResult, String> {
    let dir = install_dir().ok_or("не найдена папка LOCALAPPDATA")?;
    let exe = installed_exe().ok_or("не найдена папка LOCALAPPDATA")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let cur = std::env::current_exe().map_err(|e| e.to_string())?;
    // A running older copy must go first, or its exe cannot be replaced.
    stop_running_instance();
    if norm(&cur) != norm(&exe) {
        if exe.exists() && std::fs::remove_file(&exe).is_err() {
            let old = exe.with_extension("exe.old");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&exe, &old).map_err(|e| format!("{}: {e}", exe.display()))?;
        }
        std::fs::copy(&cur, &exe).map_err(|e| format!("копирование в {}: {e}", exe.display()))?;
    }
    if let Some(lnk) = start_menu_lnk() {
        make_shortcut(&lnk, &exe, "", "Strela — быстрые файловые операции")?;
    }
    match (opts.desktop_shortcut, desktop_lnk()) {
        (true, Some(lnk)) => make_shortcut(&lnk, &exe, "", "Strela — быстрые файловые операции")?,
        (false, Some(lnk)) => {
            let _ = std::fs::remove_file(lnk);
        }
        _ => {}
    }
    set_context_menu(opts.context_menu, &exe)?;
    sync_sendto(&exe, opts.send_to)?;
    set_autostart_for(&exe, opts.autostart)?;

    let ap = create_key(APP_PATHS_KEY)?;
    set_str(&ap, None, &exe.to_string_lossy())?;
    let u = create_key(UNINSTALL_KEY)?;
    let exe_s = exe.to_string_lossy();
    set_str(&u, Some("DisplayName"), APP)?;
    set_str(&u, Some("DisplayVersion"), env!("CARGO_PKG_VERSION"))?;
    set_str(&u, Some("Publisher"), APP)?;
    set_str(&u, Some("DisplayIcon"), &format!("{exe_s},0"))?;
    set_str(&u, Some("InstallLocation"), &dir.to_string_lossy())?;
    set_str(&u, Some("UninstallString"), &format!("\"{exe_s}\" --uninstall"))?;
    set_str(&u, Some("QuietUninstallString"), &format!("\"{exe_s}\" --uninstall --quiet"))?;
    set_str(&u, Some("URLInfoAbout"), REPO_URL)?;
    set_dword(&u, "NoModify", 1)?;
    set_dword(&u, "NoRepair", 1)?;
    set_dword(&u, "EstimatedSize", std::fs::metadata(&exe).map_or(0, |m| m.len() / 1024) as u32)?;

    let service = if opts.service { crate::service::install_via_uac(&exe).map(|_| true) } else { Ok(false) };
    Ok(InstallResult { exe, service })
}

fn set_autostart_for(exe: &Path, on: bool) -> Result<(), String> {
    if on {
        let k = create_key(RUN_KEY)?;
        set_str(&k, Some(APP), &format!("\"{}\" --hidden", exe.display()))
    } else {
        delete_value(RUN_KEY, APP);
        Ok(())
    }
}

/// Removes shortcuts, registry entries, "Send to" items, settings and (via one UAC prompt) the service; then deletes
/// the install folder once this process has exited.
pub fn uninstall() -> Result<(), String> {
    stop_running_instance();
    for lnk in [start_menu_lnk(), desktop_lnk()].into_iter().flatten() {
        let _ = std::fs::remove_file(lnk);
    }
    let _ = set_context_menu(false, Path::new(""));
    if let Some(exe) = installed_exe() {
        let _ = sync_sendto(&exe, false);
    }
    delete_value(RUN_KEY, APP);
    delete_tree(APP_PATHS_KEY);
    delete_tree(UNINSTALL_KEY);
    notify_shell();
    let mut service_note = Ok(());
    if crate::service::is_installed() {
        service_note = crate::service::uninstall_via_uac();
    }
    let _ = std::fs::remove_dir_all(crate::settings::dir());
    if let Some(dir) = install_dir()
        && dir.exists()
    {
        // The running exe cannot delete itself: a detached shell waits a moment, then removes the folder.
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd.exe")
            .raw_arg(format!("/C ping -n 3 127.0.0.1 >nul & rd /s /q \"{}\"", dir.display()))
            .creation_flags(0x0800_0000)
            .spawn();
    }
    service_note
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_layout_matches_the_spec() {
        let verbs = |t| menu_items(t).into_iter().map(|i| i.1).collect::<Vec<_>>();
        assert_eq!(verbs(Target::File), ["copy", "cut", "move-to", "fast-delete", "recycle"]);
        assert!(verbs(Target::Background).contains(&"paste"));
        assert!(verbs(Target::Drive).contains(&"largest-here"));
        for t in [Target::File, Target::Folder, Target::Background, Target::Drive] {
            let mut orders: Vec<_> = menu_items(t).into_iter().map(|i| i.0).collect();
            let n = orders.len();
            orders.sort();
            orders.dedup();
            assert_eq!(orders.len(), n, "sort keys are unique");
        }
    }

    /// Registers the menu under HKCU only, checks it, and removes it again.
    #[test]
    fn context_menu_registry_roundtrip() {
        if context_menu_enabled() {
            return; // never touch a real installation
        }
        let exe = Path::new(r"C:\Strela Test\Strela.exe");
        set_context_menu(true, exe).unwrap();
        let ok = (|| {
            for (base, arg, target) in MENU_TARGETS {
                if get_str(base, "MUIVerb")? != "Strela" || !get_str(base, "SubCommands")?.is_empty() {
                    return None;
                }
                for (order, verb, label) in menu_items(target) {
                    if get_str(&format!(r"{base}\shell\{order}"), "MUIVerb")? != label {
                        return None;
                    }
                    let cmd = get_str(&format!(r"{base}\shell\{order}\command"), "")?;
                    if cmd != format!("\"{}\" --cmd {verb} -- \"{arg}\"", exe.display()) {
                        return None;
                    }
                }
            }
            Some(())
        })();
        set_context_menu(false, exe).unwrap();
        assert!(ok.is_some(), "registry content differs from the expected layout");
        assert!(!context_menu_enabled(), "removed again");
        for (base, _, _) in MENU_TARGETS {
            assert!(!key_exists(base));
        }
    }

    #[test]
    fn autostart_roundtrip_in_hkcu() {
        let before = get_str(RUN_KEY, APP);
        if before.is_some() {
            return;
        }
        let exe = Path::new(r"C:\Strela Test\Strela.exe");
        set_autostart_for(exe, true).unwrap();
        let v = get_str(RUN_KEY, APP);
        set_autostart_for(exe, false).unwrap();
        assert_eq!(v.as_deref(), Some(r#""C:\Strela Test\Strela.exe" --hidden"#));
        assert!(get_str(RUN_KEY, APP).is_none());
    }

    #[test]
    fn shortcut_is_created_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let lnk = tmp.path().join("t.lnk");
        make_shortcut(&lnk, Path::new(r"C:\Windows\notepad.exe"), "--x", "test").unwrap();
        assert!(lnk.exists());
    }
}

//! Installed programs: the list (from the registry's Uninstall keys) and removal — either by running the program's own
//! uninstaller, silently when it can be, or by forcing it: stop its processes and services, delete its folder with the
//! fast deleter and drop its Uninstall entry. Whatever is left behind is found by [`crate::leftovers`].

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;

use crate::error::{Error, Result};
use crate::guard::Guard;
use crate::progress::Progress;
use crate::registry::{Hive, KeyPath, View};
use crate::{delete, procs, startup};

pub const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";

#[derive(Clone, Debug)]
pub struct App {
    pub key: KeyPath,
    pub name: String,
    pub version: String,
    pub publisher: String,
    pub install_location: Option<PathBuf>,
    /// `YYYY-MM-DD` when the registry has it.
    pub install_date: String,
    pub size_bytes: u64,
    pub uninstall: Option<String>,
    pub quiet_uninstall: Option<String>,
    pub icon: Option<PathBuf>,
    /// `{GUID}` of a Windows Installer (MSI) product.
    pub msi_guid: Option<String>,
    /// Full package name of a Microsoft Store (AppX / MSIX) app.
    pub store: Option<String>,
}

impl App {
    pub fn scope(&self) -> &'static str {
        if self.store.is_some() {
            return "приложение Microsoft Store";
        }
        match self.key.hive {
            Hive::User => "для текущего пользователя",
            Hive::Machine => "для всех пользователей",
        }
    }

    pub fn is_32bit(&self) -> bool {
        self.key.view == View::Wow32
    }

    /// The silent command, when one exists.
    pub fn silent_command(&self) -> Option<String> {
        if let Some(g) = &self.msi_guid {
            return Some(format!("msiexec.exe /x {g} /qn /norestart"));
        }
        self.quiet_uninstall.clone().filter(|s| !s.trim().is_empty())
    }
}

fn guid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}\}").unwrap())
}

fn update_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(security update|hotfix|update) for |\(KB\d{5,8}\)").unwrap())
}

/// Folder that holds the program: `InstallLocation`, or the folder of the uninstaller / icon when that is clearly
/// the program's own (never a drive root, `C:\Windows` or `Program Files` itself).
fn guess_location(loc: Option<String>, uninstall: &Option<String>, icon: &Option<PathBuf>) -> Option<PathBuf> {
    let guard = Guard::system();
    let usable = |p: &Path| p.is_dir() && p.parent().is_some() && guard.check(p).is_none() && !is_installer_dir(p);
    if let Some(l) = loc {
        let l = crate::registry::expand_env(l.trim().trim_matches('"'));
        let p = PathBuf::from(l.trim_end_matches('\\'));
        if !p.as_os_str().is_empty() && usable(&p) {
            return Some(p);
        }
    }
    for exe in [uninstall.as_deref().and_then(startup::exe_of_command), icon.clone()].into_iter().flatten() {
        if let Some(d) = exe.parent()
            && usable(d)
            && !exe.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().starts_with("msiexec"))
        {
            return Some(d.to_path_buf());
        }
    }
    None
}

fn is_installer_dir(p: &Path) -> bool {
    let n = crate::guard::normalize(p);
    n.contains(r"\windows\installer") || n.contains(r"\package cache") || n.ends_with(r"\system32") || n.ends_with(r"\syswow64")
}

pub fn list() -> Vec<App> {
    let spots = [(Hive::Machine, View::Native), (Hive::Machine, View::Wow32), (Hive::User, View::Native), (Hive::User, View::Wow32)];
    let mut out: Vec<App> = Vec::new();
    for (hive, view) in spots {
        let root = KeyPath::new(hive, view, UNINSTALL);
        let Some(rk) = root.open(false) else { continue };
        for sub in rk.subkeys() {
            let kp = root.child(&sub);
            let Some(k) = kp.open(false) else { continue };
            let Some(name) = k.get_string("DisplayName").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else { continue };
            if k.get_dword("SystemComponent") == Some(1) || k.get_string("ParentKeyName").is_some_and(|s| !s.is_empty()) {
                continue;
            }
            if k.get_string("ReleaseType").is_some_and(|r| matches!(r.to_lowercase().as_str(), "update" | "hotfix" | "security update" | "service pack" | "update rollup")) {
                continue;
            }
            if update_name_re().is_match(&name) {
                continue;
            }
            let uninstall = k.get_string("UninstallString").filter(|s| !s.trim().is_empty());
            let quiet = k.get_string("QuietUninstallString").filter(|s| !s.trim().is_empty());
            let icon = k.get_string("DisplayIcon").and_then(|s| startup::exe_of_command(s.split(',').next().unwrap_or("")));
            if uninstall.is_none() && quiet.is_none() && k.get_string("InstallLocation").is_none_or(|s| s.trim().is_empty()) {
                continue; // nothing we could do with it
            }
            let msi_guid = (k.get_dword("WindowsInstaller") == Some(1) || uninstall.as_deref().is_some_and(|u| u.to_lowercase().contains("msiexec")))
                .then(|| {
                    uninstall.as_deref().and_then(|u| guid_re().find(u)).map(|m| m.as_str().to_string()).or_else(|| guid_re().is_match(&sub).then(|| sub.clone()))
                })
                .flatten();
            let date = k.get_string("InstallDate").filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())).map(|d| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..])).unwrap_or_default();
            let install_location = guess_location(k.get_string("InstallLocation"), &uninstall, &icon);
            out.push(App {
                key: kp,
                name,
                version: k.get_string("DisplayVersion").unwrap_or_default(),
                publisher: k.get_string("Publisher").unwrap_or_default(),
                install_location,
                install_date: date,
                size_bytes: k.get_dword("EstimatedSize").map_or(0, |kb| kb as u64 * 1024),
                uninstall,
                quiet_uninstall: quiet,
                icon,
                msi_guid,
                store: None,
            });
        }
    }
    for p in crate::store::list() {
        out.push(App {
            key: KeyPath::new(Hive::User, View::Native, format!(r"AppX\{}", p.full_name)),
            name: p.name,
            version: p.version,
            publisher: p.publisher,
            install_location: p.install_location,
            install_date: p.install_date,
            size_bytes: 0,
            uninstall: None,
            quiet_uninstall: None,
            icon: None,
            msi_guid: None,
            store: Some(p.full_name),
        });
    }
    // the same program is often registered in both views
    let mut seen = std::collections::HashSet::new();
    out.retain(|a| seen.insert((a.name.to_lowercase(), a.version.clone(), a.publisher.to_lowercase(), a.uninstall.clone().unwrap_or_default().to_lowercase())));
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The program's own uninstaller without windows when it can (MSI always; `QuietUninstallString` otherwise).
    Quiet,
    /// The program's own uninstaller, with its dialogs.
    Interactive,
    /// No uninstaller: stop it, delete its folder, drop its entry. (MSI products still go through `msiexec /qn`,
    /// otherwise Windows Installer would keep a phantom product.)
    Force,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub killed: usize,
    pub services_removed: usize,
    pub folder_removed: Option<PathBuf>,
    pub bytes: u64,
    pub elapsed: Duration,
    pub uninstaller_exit: Option<u32>,
    /// The Uninstall entry is still there afterwards (the uninstaller may still be running, or failed).
    pub still_registered: bool,
    pub notes: Vec<String>,
}

fn run_command_line(cmd: &str, wait: Option<Duration>, progress: &Progress) -> Result<Option<u32>> {
    use std::os::windows::process::CommandExt;
    let mut child = std::process::Command::new("cmd.exe").arg("/C").raw_arg(format!("\"{cmd}\"")).creation_flags(0x0800_0000).spawn()?;
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(st.code().map(|c| c as u32));
        }
        if progress.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if wait.is_some_and(|w| start.elapsed() > w) {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_unregistered(app: &App, up_to: Duration, progress: &Progress) -> bool {
    let start = Instant::now();
    loop {
        if !app.key.exists() {
            return true;
        }
        if start.elapsed() > up_to || progress.is_cancelled() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Stops everything that runs from `dir`: processes and services.
pub fn stop_running_from(dir: &Path, report: &mut Report, log: &mut dyn FnMut(&str)) {
    // services first, so the SCM does not restart what is killed
    for e in startup::list_services().into_iter() {
        if let startup::Kind::Service { name, running, .. } = &e.kind
            && e.path.as_ref().is_some_and(|p| under(p, dir))
        {
            if *running {
                log(&format!("останавливаю службу {name}"));
            }
            if startup::delete_service(name).is_ok() {
                report.services_removed += 1;
            } else {
                report.notes.push(format!("служба {name} не удалена (нужны права администратора)"));
            }
        }
    }
    let mut sampler = procs::Sampler::new();
    if let Ok(all) = sampler.snapshot() {
        let pids = procs::running_from(dir, &all);
        let mut tree: Vec<u32> = Vec::new();
        for pid in pids {
            for t in procs::tree_pids(pid, &all) {
                if !tree.contains(&t) {
                    tree.push(t);
                }
            }
        }
        if !tree.is_empty() {
            log(&format!("завершаю процессы: {}", tree.len()));
            let (done, failed) = procs::terminate_many(&tree, &all, sampler.own_pid());
            report.killed += done;
            for (pid, why) in failed {
                report.notes.push(format!("процесс {pid} не завершён: {why}"));
            }
            std::thread::sleep(Duration::from_millis(150)); // let the files unlock
        }
    }
}

pub fn under(p: &Path, dir: &Path) -> bool {
    let (n, d) = (crate::guard::normalize(p), crate::guard::normalize(dir));
    n == d || (n.len() > d.len() && n.starts_with(&d) && n.as_bytes()[d.len()] == b'\\')
}

pub fn uninstall(app: &App, mode: Mode, progress: &Progress, log: &mut dyn FnMut(&str)) -> Result<Report> {
    let t0 = Instant::now();
    let mut rep = Report::default();
    if let Some(full) = &app.store {
        if let Some(dir) = &app.install_location {
            stop_running_from(dir, &mut rep, log);
        }
        log("удаляю приложение Microsoft Store");
        crate::store::remove(full)?;
        rep.elapsed = t0.elapsed();
        return Ok(rep);
    }
    let forced = mode == Mode::Force && app.msi_guid.is_none();
    if mode != Mode::Interactive
        && let Some(dir) = &app.install_location
    {
        stop_running_from(dir, &mut rep, log);
    }
    if !forced {
        let cmd = match mode {
            Mode::Interactive => app.uninstall.clone().or_else(|| app.silent_command()),
            _ => app.silent_command().or_else(|| app.uninstall.clone()),
        };
        let Some(cmd) = cmd else {
            return Err(Error::Unsupported("у программы нет команды удаления — используйте принудительное удаление".into()));
        };
        let silent = app.silent_command().as_deref() == Some(cmd.as_str());
        log(&format!("запускаю деинсталлятор{}: {cmd}", if silent { " (тихо)" } else { "" }));
        let wait = if mode == Mode::Interactive { None } else { Some(Duration::from_secs(900)) };
        rep.uninstaller_exit = run_command_line(&cmd, wait, progress)?;
        // many uninstallers hand over to a copy of themselves and exit at once
        rep.still_registered = !wait_unregistered(app, Duration::from_secs(if mode == Mode::Interactive { 5 } else { 20 }), progress);
        if rep.still_registered {
            rep.notes.push("запись о программе ещё в списке: деинсталлятор мог не закончить работу или завершился с ошибкой".into());
        }
    } else {
        if app.msi_guid.is_none() && mode == Mode::Force {
            log("принудительное удаление: без деинсталлятора");
        }
        if let Some(dir) = &app.install_location {
            let guard = Guard::system();
            let pv = delete::preview(std::slice::from_ref(dir), &guard, progress)?;
            let bytes = pv.bytes;
            let c = pv.confirm(None)?;
            log(&format!("удаляю папку {}", crate::winapi::display_path(dir)));
            let r = delete::delete_permanently(&c, progress)?;
            rep.bytes = bytes;
            if r.failed.is_empty() {
                rep.folder_removed = Some(dir.clone());
            } else {
                for (p, m) in r.failed.iter().take(5) {
                    rep.notes.push(format!("не удалось удалить {}: {m}", crate::winapi::display_path(p)));
                }
            }
        } else {
            rep.notes.push("папка установки неизвестна — удалена только запись о программе; остальное найдёт поиск следов".into());
        }
        // the Uninstall entry (saved to a .reg first)
        let backup = crate::leftovers::backup_dir(&app.name);
        let _ = std::fs::create_dir_all(&backup);
        let _ = crate::registry::export(&app.key, &backup.join("uninstall-entry.reg"));
        match crate::registry::delete_tree(&app.key) {
            Ok(()) => {}
            Err(e) => rep.notes.push(format!("запись {} не удалена: {e}", app.key.display())),
        }
        rep.still_registered = app.key.exists();
    }
    rep.elapsed = t0.elapsed();
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_programs_with_names() {
        let apps = list();
        // a normal machine has programs; the list is sorted and every entry has a name and something to act on
        assert!(!apps.is_empty());
        assert!(apps.windows(2).all(|w| w[0].name.to_lowercase() <= w[1].name.to_lowercase()));
        assert!(apps.iter().all(|a| !a.name.is_empty()));
        assert!(apps.iter().all(|a| a.install_location.as_ref().is_none_or(|p| Guard::system().check(p).is_none())), "never a protected folder as the install location");
    }

    #[test]
    fn msi_command_and_guid() {
        let g = "{3F2504E0-4F89-11D3-9A0C-0305E82C3301}";
        assert!(guid_re().is_match(&format!("MsiExec.exe /I{g}")));
        let app = App {
            key: KeyPath::new(Hive::User, View::Native, "x"),
            name: "x".into(),
            version: String::new(),
            publisher: String::new(),
            install_location: None,
            install_date: String::new(),
            size_bytes: 0,
            uninstall: Some(format!("MsiExec.exe /I{g}")),
            quiet_uninstall: None,
            icon: None,
            msi_guid: Some(g.into()),
            store: None,
        };
        assert_eq!(app.silent_command().unwrap(), format!("msiexec.exe /x {g} /qn /norestart"));
    }

    #[test]
    fn update_names_are_filtered() {
        assert!(update_name_re().is_match("Security Update for Microsoft Word (KB5002123)"));
        assert!(update_name_re().is_match("Hotfix for Foo"));
        assert!(!update_name_re().is_match("Update Manager Pro"));
    }

    #[test]
    fn under_is_exact_about_boundaries() {
        assert!(under(Path::new(r"C:\Apps\Foo\bin\x.exe"), Path::new(r"C:\Apps\Foo")));
        assert!(under(Path::new(r"c:\apps\foo"), Path::new(r"C:\Apps\Foo\")));
        assert!(!under(Path::new(r"C:\Apps\Foobar\x.exe"), Path::new(r"C:\Apps\Foo")));
    }

    #[test]
    fn forced_removal_of_a_fake_program() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("FakeApp");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("a.bin"), vec![0u8; 4096]).unwrap();
        let id = format!("StrelaFake-{}", std::process::id());
        let key = KeyPath::new(Hive::User, View::Native, format!(r"{UNINSTALL}\{id}"));
        let rk = crate::registry::RegKey::create(Hive::User, View::Native, &key.path).unwrap();
        let s: Vec<u8> = "StrelaFake".encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
        rk.set_raw("DisplayName", 1, &s).unwrap();
        drop(rk);
        let app = App {
            key: key.clone(),
            name: "StrelaFake".into(),
            version: "1.0".into(),
            publisher: String::new(),
            install_location: Some(dir.clone()),
            install_date: String::new(),
            size_bytes: 0,
            uninstall: None,
            quiet_uninstall: None,
            icon: None,
            msi_guid: None,
            store: None,
        };
        let mut lines = Vec::new();
        let rep = uninstall(&app, Mode::Force, &Progress::default(), &mut |s| lines.push(s.to_string())).unwrap();
        assert_eq!(rep.folder_removed.as_deref(), Some(dir.as_path()), "{rep:?}");
        assert!(!dir.exists());
        assert!(!key.exists());
        assert!(!rep.still_registered);
        assert!(rep.bytes >= 4096);
        let _ = std::fs::remove_dir_all(crate::leftovers::backup_dir("StrelaFake"));
    }
}

//! Traces of a program: what stays in the system after (or without) its uninstaller — folders in Program Files /
//! ProgramData / AppData, shortcuts, registry keys, autostart entries, services and scheduled tasks.
//!
//! Matching is deliberately conservative. A trace is **High** confidence when it lies inside the program's install
//! folder, points into it, or is named exactly like the program (or like its install folder); it is **Medium** when
//! only a distinctive word of the program's name matches. The GUI pre-selects High only. Registry keys and values are
//! exported to `.reg` files before they are deleted, and nothing protected by [`Guard`] is ever touched.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rayon::prelude::*;
use regex::Regex;

use crate::apps::{self, App, under};
use crate::guard::Guard;
use crate::progress::Progress;
use crate::registry::{Hive, KeyPath, View};
use crate::{delete, startup, winapi};

#[derive(Clone, Debug)]
pub struct Subject {
    pub name: String,
    pub publisher: String,
    pub install_location: Option<PathBuf>,
    /// Its Uninstall key, when known.
    pub key: Option<KeyPath>,
    pub msi_guid: Option<String>,
}

impl Subject {
    pub fn from_app(a: &App) -> Subject {
        Subject { name: a.name.clone(), publisher: a.publisher.clone(), install_location: a.install_location.clone(), key: Some(a.key.clone()), msi_guid: a.msi_guid.clone() }
    }

    /// For a program that is already gone: only its name (and perhaps the folder it was in).
    pub fn by_name(name: &str) -> Subject {
        Subject { name: name.trim().to_string(), publisher: String::new(), install_location: None, key: None, msi_guid: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    High,
    Medium,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Path(PathBuf),
    RegKey(KeyPath),
    RegValue { key: KeyPath, name: String },
    Service(String),
    Task(String),
}

#[derive(Clone, Debug)]
pub struct Trace {
    pub item: Item,
    pub label: String,
    pub kind: &'static str,
    pub reason: String,
    pub confidence: Confidence,
    pub size: u64,
}

// ---------------------------------------------------------------------------------------------------------
// Names

/// Lowercase letters and digits only: `Visual-Studio Code` and `visual studio code` meet.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

fn strip_re() -> &'static (Regex, Regex) {
    static RE: OnceLock<(Regex, Regex)> = OnceLock::new();
    RE.get_or_init(|| {
        (
            Regex::new(r"(?i)\s*[\(\[][^)\]]*(x64|x86|64-bit|32-bit|64 bit|32 bit|en-us|ru-ru|\d+\.\d+)[^)\]]*[\)\]]").unwrap(),
            Regex::new(r"(?i)[\s_-]+(v(ersion)?\s*)?\d+([._]\d+)*[a-z]?(\s+(x64|x86|64-bit|32-bit))?$").unwrap(),
        )
    })
}

/// `Mozilla Firefox (x64 en-US)` -> `Mozilla Firefox`; `7-Zip 23.01 (x64)` -> `7-Zip`.
pub fn clean_name(name: &str) -> String {
    let (parens, version) = strip_re();
    let mut s = parens.replace_all(name, "").trim().to_string();
    for _ in 0..3 {
        let t = version.replace(&s, "").trim().to_string();
        if t == s || t.is_empty() {
            break;
        }
        s = t;
    }
    s
}

const GENERIC: &[&str] = &[
    "microsoft", "windows", "google", "adobe", "intel", "nvidia", "amd", "apple", "oracle", "java", "python", "office", "visual", "studio", "common", "files", "software", "system",
    "driver", "drivers", "update", "updates", "runtime", "redistributable", "program", "programs", "app", "apps", "application", "data", "temp", "tools", "tool", "service", "services",
    "client", "server", "manager", "launcher", "installer", "setup", "classes", "policies", "clients", "microsoftcorporation", "corporation", "inc", "ltd", "llc", "games", "game",
    "media", "player", "browser", "desktop", "pro", "free", "plus", "edition", "wow6432node", "packages", "package", "cache", "intellij", "network", "security", "security", "cloud",
];

struct Names {
    strong: HashSet<String>,
    weak: HashSet<String>,
    publisher: String,
}

fn usable(n: &str) -> bool {
    n.chars().count() >= 3 && !GENERIC.contains(&n)
}

impl Names {
    fn of(s: &Subject) -> Names {
        let mut strong = HashSet::new();
        let mut weak = HashSet::new();
        let publisher = norm(&clean_name(&s.publisher));
        let full = clean_name(&s.name);
        let n = norm(&full);
        if usable(&n) {
            strong.insert(n);
        }
        let n2 = norm(&s.name);
        if usable(&n2) {
            strong.insert(n2);
        }
        if let Some(dir) = s.install_location.as_ref().and_then(|p| p.file_name()) {
            let n = norm(&dir.to_string_lossy());
            if usable(&n) {
                strong.insert(n);
            }
        }
        // no publisher prefix: `Mozilla Firefox` also leaves `Firefox`
        if !publisher.is_empty() {
            for st in strong.clone() {
                if let Some(rest) = st.strip_prefix(&publisher)
                    && usable(rest)
                    && rest.len() >= 4
                {
                    weak.insert(rest.to_string());
                }
            }
        }
        // the first word of a multi-word name is usually the vendor (`Mozilla Firefox`): shared with its other products
        let words: Vec<&str> = full.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
        for w in words.iter().skip(usize::from(words.len() > 1)) {
            let n = norm(w);
            if n.len() >= 5 && usable(&n) && n != publisher && !publisher.contains(&n) {
                weak.insert(n);
            }
        }
        for st in &strong {
            weak.remove(st);
        }
        Names { strong, weak, publisher }
    }

    fn classify(&self, entry_name: &str) -> Option<(Confidence, &'static str)> {
        let n = norm(entry_name);
        if n.is_empty() {
            return None;
        }
        if self.strong.contains(&n) {
            return Some((Confidence::High, "название совпадает с названием программы"));
        }
        if self.weak.contains(&n) {
            return Some((Confidence::Medium, "совпадает часть названия программы"));
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------------------
// Sizes

fn dir_size(path: &Path, progress: &Progress) -> u64 {
    if progress.is_cancelled() {
        return 0;
    }
    let mut kids: Vec<(PathBuf, bool, bool, u64)> = Vec::new();
    let _ = winapi::list_dir(path, |e| {
        use std::os::windows::ffi::OsStringExt;
        kids.push((path.join(std::ffi::OsString::from_wide(e.name)), e.is_dir(), e.is_reparse(), e.size));
    });
    kids.par_iter().map(|(p, dir, reparse, size)| if *reparse { 0 } else if *dir { dir_size(p, progress) } else { *size }).sum()
}

fn path_size(p: &Path, progress: &Progress) -> u64 {
    match std::fs::symlink_metadata(winapi::long_path(p)) {
        Ok(m) if m.is_dir() => dir_size(p, progress),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------------------------------------
// Scan

struct Found {
    items: Vec<Trace>,
    seen: HashSet<Item>,
}

impl Found {
    fn add(&mut self, item: Item, label: String, kind: &'static str, reason: &str, confidence: Confidence) {
        if self.seen.insert(item.clone()) {
            self.items.push(Trace { item, label, kind, reason: reason.to_string(), confidence, size: 0 });
        }
    }
}

fn env_dirs(vars: &[&str]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    for var in vars {
        if let Some(p) = std::env::var_os(var).map(PathBuf::from)
            && p.is_dir()
            && !v.iter().any(|x| crate::guard::normalize(x) == crate::guard::normalize(&p))
        {
            v.push(p);
        }
    }
    v
}

fn folder_roots() -> Vec<(PathBuf, bool)> {
    // (folder, matches are only Medium: shared scratch space)
    let mut roots: Vec<(PathBuf, bool)> = env_dirs(&["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData", "APPDATA", "LOCALAPPDATA"]).into_iter().map(|p| (p, false)).collect();
    if let Some(l) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        roots.push((l.join("Programs"), false));
        roots.push((l.join("Temp"), true));
    }
    if let Some(u) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
        roots.push((u.join(r"AppData\LocalLow"), false));
    }
    if let Some(a) = std::env::var_os("APPDATA").map(PathBuf::from) {
        roots.push((a.join(r"Microsoft\Windows\Start Menu\Programs"), false));
    }
    if let Some(a) = std::env::var_os("ProgramData").map(PathBuf::from) {
        roots.push((a.join(r"Microsoft\Windows\Start Menu\Programs"), false));
    }
    for d in env_dirs(&["USERPROFILE", "PUBLIC"]) {
        roots.push((d.join("Desktop"), false));
    }
    roots.retain(|(p, _)| p.is_dir());
    roots
}

fn scan_folders(names: &Names, found: &mut Found) {
    let guard = Guard::system();
    for (root, shared_scratch) in folder_roots() {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        let kids: Vec<(PathBuf, String, bool)> = rd.flatten().map(|e| (e.path(), e.file_name().to_string_lossy().into_owned(), e.path().is_dir())).collect();
        for (p, fname, is_dir) in &kids {
            let stem = if *is_dir { fname.clone() } else { Path::new(fname).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default() };
            if !is_dir && !fname.to_lowercase().ends_with(".lnk") {
                continue;
            }
            if let Some((mut conf, why)) = names.classify(&stem) {
                if shared_scratch {
                    conf = Confidence::Medium;
                }
                if guard.check(p).is_none() {
                    found.add(Item::Path(p.clone()), winapi::display_path(p), if *is_dir { "Папка" } else { "Ярлык" }, why, conf);
                }
            } else if *is_dir && !names.publisher.is_empty() && usable(&names.publisher) && norm(fname) == names.publisher {
                // <root>\<Publisher>\<Program>: take the publisher folder along only when nothing else is in it
                let Ok(sub) = std::fs::read_dir(p) else { continue };
                let subs: Vec<(PathBuf, String)> = sub.flatten().filter(|e| e.path().is_dir()).map(|e| (e.path(), e.file_name().to_string_lossy().into_owned())).collect();
                let all_entries = std::fs::read_dir(p).map_or(0, |r| r.count());
                let hits: Vec<&(PathBuf, String)> = subs.iter().filter(|(_, n)| names.classify(n).is_some_and(|(c, _)| c == Confidence::High)).collect();
                if !hits.is_empty() && hits.len() == all_entries && guard.check(p).is_none() {
                    found.add(Item::Path(p.clone()), winapi::display_path(p), "Папка", "папка издателя, в которой только эта программа", Confidence::High);
                } else {
                    for (sp, sn) in hits {
                        let _ = sn;
                        found.add(Item::Path(sp.clone()), winapi::display_path(sp), "Папка", "папка программы внутри папки издателя", Confidence::High);
                    }
                }
            }
        }
    }
}

fn reg_roots() -> Vec<KeyPath> {
    vec![
        KeyPath::new(Hive::User, View::Native, "Software"),
        KeyPath::new(Hive::Machine, View::Native, "SOFTWARE"),
        KeyPath::new(Hive::Machine, View::Wow32, "SOFTWARE"),
    ]
}

fn reg_protected(k: &KeyPath) -> bool {
    // never the top of the tree, never anything outside Software\*\*
    let parts: Vec<&str> = k.path.split('\\').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return true;
    }
    let first = norm(parts[0]);
    if first == "software" {
        let second = norm(parts[1]);
        return matches!(second.as_str(), "microsoft" | "classes" | "policies" | "wow6432node" | "clients" | "registeredapplications" | "windows" | "intel" | "nvidia" | "amd" | "google" | "adobe" | "apple" | "oracle");
    }
    false
}

fn scan_registry(names: &Names, subject: &Subject, found: &mut Found) {
    for root in reg_roots() {
        let Some(rk) = root.open(false) else { continue };
        for sub in rk.subkeys() {
            let kp = root.child(&sub);
            if let Some((conf, why)) = names.classify(&sub) {
                if !reg_protected(&kp) {
                    found.add(Item::RegKey(kp), format!("{}\\{sub}", root.display()), "Ключ реестра", why, conf);
                }
            } else if !names.publisher.is_empty() && usable(&names.publisher) && norm(&sub) == names.publisher {
                let Some(pk) = kp.open(false) else { continue };
                let inner = pk.subkeys();
                let hits: Vec<&String> = inner.iter().filter(|n| names.classify(n).is_some_and(|(c, _)| c == Confidence::High)).collect();
                if !hits.is_empty() && hits.len() == inner.len() && pk.values().is_empty() {
                    found.add(Item::RegKey(kp.clone()), format!("{}\\{sub}", root.display()), "Ключ реестра", "ключ издателя, в котором только эта программа", Confidence::High);
                } else {
                    for h in hits {
                        let k2 = kp.child(h);
                        found.add(Item::RegKey(k2.clone()), k2.display(), "Ключ реестра", "ключ программы внутри ключа издателя", Confidence::High);
                    }
                }
            }
        }
    }
    // Uninstall entries: its own, and any other that points into the install folder
    for (hive, view) in [(Hive::Machine, View::Native), (Hive::Machine, View::Wow32), (Hive::User, View::Native), (Hive::User, View::Wow32)] {
        let root = KeyPath::new(hive, view, apps::UNINSTALL);
        let Some(rk) = root.open(false) else { continue };
        for sub in rk.subkeys() {
            let kp = root.child(&sub);
            let own = subject.key.as_ref() == Some(&kp) || subject.msi_guid.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(&sub));
            let Some(k) = kp.open(false) else { continue };
            let dn = k.get_string("DisplayName").unwrap_or_default();
            let loc_hit = subject.install_location.as_ref().is_some_and(|d| {
                k.get_string("InstallLocation").is_some_and(|l| !l.trim().is_empty() && under(Path::new(l.trim().trim_matches('"')), d))
                    || k.get_string("UninstallString").and_then(|u| startup::exe_of_command(&u)).is_some_and(|e| under(&e, d))
            });
            if own {
                found.add(Item::RegKey(kp.clone()), kp.display(), "Запись об удалении", "запись программы в списке «Программы и компоненты»", Confidence::High);
            } else if loc_hit {
                found.add(Item::RegKey(kp.clone()), kp.display(), "Запись об удалении", "указывает в папку программы", Confidence::High);
            } else if !dn.is_empty() && names.classify(&clean_name(&dn)).is_some_and(|(c, _)| c == Confidence::High) {
                found.add(Item::RegKey(kp.clone()), kp.display(), "Запись об удалении", "то же название программы", Confidence::High);
            }
        }
    }
    // App Paths pointing into the folder
    if let Some(dir) = &subject.install_location {
        for view in [View::Native, View::Wow32] {
            for hive in [Hive::Machine, Hive::User] {
                let root = KeyPath::new(hive, view, r"Software\Microsoft\Windows\CurrentVersion\App Paths");
                let Some(rk) = root.open(false) else { continue };
                for sub in rk.subkeys() {
                    let kp = root.child(&sub);
                    let hit = kp.open(false).is_some_and(|k| k.get_string("").or_else(|| k.get_string("Path")).is_some_and(|v| under(Path::new(v.trim().trim_matches('"')), dir)));
                    if hit {
                        found.add(Item::RegKey(kp.clone()), kp.display(), "Ключ реестра", "App Paths: путь к программе", Confidence::High);
                    }
                }
            }
        }
    }
}

fn scan_startup(names: &Names, subject: &Subject, found: &mut Found) {
    let dir = subject.install_location.as_deref();
    for e in startup::list() {
        let inside = dir.is_some_and(|d| e.path.as_ref().is_some_and(|p| under(p, d)));
        let by_name = names.classify(&clean_name(&e.name)).filter(|(c, _)| *c == Confidence::High);
        let (conf, why) = if inside {
            (Confidence::High, "запускает файл из папки программы")
        } else if let Some((_, w)) = by_name {
            if e.system {
                continue;
            }
            (Confidence::Medium, w)
        } else {
            continue;
        };
        match &e.kind {
            startup::Kind::RunValue { key, value, .. } => found.add(Item::RegValue { key: key.clone(), name: value.clone() }, format!("{} → {}", key.display(), e.name), "Автозагрузка", why, conf),
            startup::Kind::FolderItem { path, .. } => found.add(Item::Path(path.clone()), winapi::display_path(path), "Автозагрузка", why, conf),
            startup::Kind::Service { name, .. } if !e.system => found.add(Item::Service(name.clone()), format!("служба {name}"), "Служба", why, conf),
            startup::Kind::Task { name, .. } if !e.system => found.add(Item::Task(name.clone()), format!("задача {name}"), "Задача", why, conf),
            _ => {}
        }
    }
    // services and tasks that are not auto-start but still point into the folder are caught by the registry / task scan below
    if let Some(d) = dir {
        for e in startup::list_services() {
            if let startup::Kind::Service { name, .. } = &e.kind
                && e.path.as_ref().is_some_and(|p| under(p, d))
            {
                found.add(Item::Service(name.clone()), format!("служба {name}"), "Служба", "запускает файл из папки программы", Confidence::High);
            }
        }
    }
}

/// Finds traces. `progress.items` counts what has been found; the call can be cancelled.
pub fn scan(subject: &Subject, progress: &Progress) -> Vec<Trace> {
    let names = Names::of(subject);
    let mut found = Found { items: Vec::new(), seen: HashSet::new() };
    let guard = Guard::system();
    if let Some(d) = &subject.install_location
        && d.exists()
        && guard.check(d).is_none()
    {
        found.add(Item::Path(d.clone()), winapi::display_path(d), "Папка", "папка установки", Confidence::High);
    }
    if let Some(g) = &subject.msi_guid
        && let Some(pd) = std::env::var_os("ProgramData")
    {
        let cache = PathBuf::from(pd).join("Package Cache").join(g);
        if cache.is_dir() {
            found.add(Item::Path(cache.clone()), winapi::display_path(&cache), "Папка", "кэш установщика этой программы", Confidence::High);
        }
    }
    scan_folders(&names, &mut found);
    scan_registry(&names, subject, &mut found);
    scan_startup(&names, subject, &mut found);
    // drop paths lying inside another found path
    let paths: Vec<PathBuf> = found.items.iter().filter_map(|t| if let Item::Path(p) = &t.item { Some(p.clone()) } else { None }).collect();
    found.items.retain(|t| match &t.item {
        Item::Path(p) => !paths.iter().any(|o| o != p && under(p, o)),
        _ => true,
    });
    let mut keys: Vec<KeyPath> = found.items.iter().filter_map(|t| if let Item::RegKey(k) = &t.item { Some(k.clone()) } else { None }).collect();
    keys.sort();
    found.items.retain(|t| match &t.item {
        Item::RegKey(k) => !keys.iter().any(|o| o != k && o.hive == k.hive && o.view == k.view && k.path.len() > o.path.len() && k.path.starts_with(&o.path) && k.path.as_bytes()[o.path.len()] == b'\\'),
        _ => true,
    });
    found.items.par_iter_mut().for_each(|t| {
        if let Item::Path(p) = &t.item {
            t.size = path_size(p, progress);
        }
    });
    progress.items.store(found.items.len() as u64, std::sync::atomic::Ordering::Relaxed);
    found.items.sort_by(|a, b| a.confidence.cmp(&b.confidence).then(a.kind.cmp(b.kind)).then(a.label.to_lowercase().cmp(&b.label.to_lowercase())));
    found.items
}

// ---------------------------------------------------------------------------------------------------------
// Clean

pub fn backup_dir(app: &str) -> PathBuf {
    let safe: String = app.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
    let stamp = crate::util::date(now).replace([':', ' '], "-");
    crate::util::data_dir().join("backups").join(format!("{stamp} {}", safe.trim()))
}

#[derive(Clone, Debug, Default)]
pub struct CleanReport {
    pub removed: usize,
    pub bytes: u64,
    pub failed: Vec<(String, String)>,
    pub backup: Option<PathBuf>,
    pub elapsed: std::time::Duration,
}

/// Removes the given traces. Folders and files go through the fast deleter in one batch.
pub fn clean(traces: &[Trace], app_name: &str, progress: &Progress) -> CleanReport {
    let t0 = std::time::Instant::now();
    let mut rep = CleanReport::default();
    let backup = backup_dir(app_name);
    let mut backed_up = false;
    let mut backup_n = 0;
    let mut save_key = |k: &KeyPath, rep: &mut CleanReport| {
        if std::fs::create_dir_all(&backup).is_ok() {
            backup_n += 1;
            let _ = crate::registry::export(k, &backup.join(format!("{backup_n:03}.reg")));
            backed_up = true;
            rep.backup = Some(backup.clone());
        }
    };
    let guard = Guard::system();
    let mut paths: Vec<PathBuf> = Vec::new();
    for t in traces {
        match &t.item {
            Item::Path(p) => {
                if guard.check(p).is_some() {
                    rep.failed.push((t.label.clone(), "защищённое место — не трогаю".into()));
                } else if std::fs::symlink_metadata(winapi::long_path(p)).is_ok() {
                    paths.push(p.clone());
                } else {
                    rep.removed += 1; // already gone
                }
            }
            Item::RegKey(k) => {
                if reg_protected(k) && !k.path.to_lowercase().starts_with(&apps::UNINSTALL.to_lowercase()) && !k.path.to_lowercase().contains("app paths") {
                    rep.failed.push((t.label.clone(), "защищённый раздел реестра — не трогаю".into()));
                    continue;
                }
                save_key(k, &mut rep);
                match crate::registry::delete_tree(k) {
                    Ok(()) => rep.removed += 1,
                    Err(e) => rep.failed.push((t.label.clone(), if e.kind() == std::io::ErrorKind::PermissionDenied { "нужны права администратора".into() } else { e.to_string() })),
                }
            }
            Item::RegValue { key, name } => {
                save_key(key, &mut rep);
                match key.open(true).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied)).and_then(|k| k.delete_value(name)) {
                    Ok(()) => rep.removed += 1,
                    Err(e) => rep.failed.push((t.label.clone(), if e.kind() == std::io::ErrorKind::PermissionDenied { "нужны права администратора".into() } else { e.to_string() })),
                }
            }
            Item::Service(name) => {
                save_key(&KeyPath::new(Hive::Machine, View::Native, format!(r"SYSTEM\CurrentControlSet\Services\{name}")), &mut rep);
                match startup::delete_service(name) {
                    Ok(()) => rep.removed += 1,
                    Err(e) => rep.failed.push((t.label.clone(), if e.raw_os_error() == Some(5) { "нужны права администратора".into() } else { e.to_string() })),
                }
            }
            Item::Task(name) => match startup::delete_task(name) {
                Ok(()) => rep.removed += 1,
                Err(e) => rep.failed.push((t.label.clone(), e.to_string())),
            },
        }
    }
    let _ = backed_up;
    if !paths.is_empty() {
        match delete::preview(&paths, &guard, progress).and_then(|pv| {
            let bytes = pv.bytes;
            pv.confirm(None).map(|c| (c, bytes))
        }) {
            Ok((c, bytes)) => match delete::delete_permanently(&c, progress) {
                Ok(r) => {
                    rep.bytes += bytes;
                    let bad: HashSet<PathBuf> = r.failed.iter().map(|(p, _)| p.clone()).collect();
                    for p in &paths {
                        if !bad.iter().any(|b| under(b, p)) {
                            rep.removed += 1;
                        }
                    }
                    for (p, m) in r.failed {
                        rep.failed.push((winapi::display_path(&p), m));
                    }
                }
                Err(e) => rep.failed.push(("файлы и папки".into(), e.to_string())),
            },
            Err(e) => rep.failed.push(("файлы и папки".into(), e.to_string())),
        }
    }
    rep.elapsed = t0.elapsed();
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::RegKey;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("Mozilla Firefox (x64 en-US)"), "Mozilla Firefox");
        assert_eq!(clean_name("7-Zip 23.01 (x64)"), "7-Zip");
        assert_eq!(clean_name("Notepad++ (64-bit x64)"), "Notepad++");
        assert_eq!(clean_name("Foo Bar v2.3.1"), "Foo Bar");
        assert_eq!(clean_name("Python 3.12.1 (64-bit)"), "Python");
        assert_eq!(clean_name("Origin"), "Origin");
    }

    #[test]
    fn matching_is_conservative() {
        let s = Subject { name: "Mozilla Firefox (x64 en-US)".into(), publisher: "Mozilla".into(), install_location: Some(PathBuf::from(r"C:\Program Files\Mozilla Firefox")), key: None, msi_guid: None };
        let n = Names::of(&s);
        assert_eq!(n.classify("Mozilla Firefox").unwrap().0, Confidence::High);
        assert_eq!(n.classify("mozilla-firefox").unwrap().0, Confidence::High);
        assert_eq!(n.classify("Firefox").unwrap().0, Confidence::Medium);
        // without a known publisher the vendor word (first word) is still not a match on its own
        let bare = Names::of(&Subject::by_name("Strela Demo App"));
        assert!(bare.classify("Strela").is_none(), "vendor word alone");
        assert_eq!(bare.classify("StrelaDemoApp").unwrap().0, Confidence::High);
        assert!(n.classify("Mozilla").is_none(), "the publisher alone is shared with other products");
        assert!(n.classify("Thunderbird").is_none());
        assert!(n.classify("Microsoft").is_none());
        // too short / generic names produce nothing
        let g = Names::of(&Subject::by_name("Go"));
        assert!(g.strong.is_empty() && g.weak.is_empty());
        let w = Names::of(&Subject::by_name("Windows Update Tools"));
        assert!(w.classify("Windows").is_none());
    }

    #[test]
    fn registry_protection() {
        let k = |p: &str| KeyPath::new(Hive::Machine, View::Native, p);
        assert!(reg_protected(&k("SOFTWARE")));
        assert!(reg_protected(&k(r"SOFTWARE\Microsoft")));
        assert!(reg_protected(&k(r"SOFTWARE\Classes")));
        assert!(!reg_protected(&k(r"SOFTWARE\SomeVendor")));
        assert!(!reg_protected(&k(r"SOFTWARE\SomeVendor\SomeApp")));
    }

    /// End to end on throw-away data: folder, shortcut-like file, registry key and a Run value are found and removed.
    #[test]
    fn scan_and_clean_roundtrip() {
        let uniq = format!("StrelaZzTest{}", std::process::id());
        let appdata = PathBuf::from(std::env::var_os("APPDATA").unwrap());
        let folder = appdata.join(&uniq);
        std::fs::create_dir_all(folder.join("cache")).unwrap();
        std::fs::write(folder.join("cache").join("blob"), vec![1u8; 10_000]).unwrap();
        let soft = KeyPath::new(Hive::User, View::Native, format!(r"Software\{uniq}"));
        RegKey::create(Hive::User, View::Native, &format!(r"Software\{uniq}\Settings")).unwrap();
        let run = KeyPath::new(Hive::User, View::Native, r"Software\Microsoft\Windows\CurrentVersion\Run");
        let fake_exe = folder.join("app.exe");
        std::fs::write(&fake_exe, b"MZ").unwrap();
        {
            let rk = run.open(true).unwrap();
            let v: Vec<u8> = format!("\"{}\"", fake_exe.display()).encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
            rk.set_raw(&format!("{uniq}Updater"), 1, &v).unwrap();
        }
        let subject = Subject { name: uniq.clone(), publisher: String::new(), install_location: None, key: None, msi_guid: None };
        let found = scan(&subject, &Progress::default());
        let has = |f: &dyn Fn(&Item) -> bool| found.iter().any(|t| f(&t.item));
        assert!(has(&|i| matches!(i, Item::Path(p) if *p == folder)), "{found:#?}");
        assert!(has(&|i| matches!(i, Item::RegKey(k) if *k == soft)));
        assert!(found.iter().all(|t| t.confidence == Confidence::High));
        let folder_trace = found.iter().find(|t| matches!(&t.item, Item::Path(p) if *p == folder)).unwrap();
        assert!(folder_trace.size >= 10_000);
        // the Run value points into the folder only when the folder is the install location
        let subject2 = Subject { install_location: Some(folder.clone()), ..subject.clone() };
        let found2 = scan(&subject2, &Progress::default());
        assert!(found2.iter().any(|t| matches!(&t.item, Item::RegValue { name, .. } if name == &format!("{uniq}Updater"))), "{found2:#?}");
        let rep = clean(&found2, &uniq, &Progress::default());
        assert!(rep.failed.is_empty(), "{rep:?}");
        assert!(!folder.exists());
        assert!(!soft.exists());
        assert!(run.open(false).unwrap().get(&format!("{uniq}Updater")).is_none());
        assert!(rep.backup.as_ref().is_some_and(|b| b.exists()), "registry keys are exported first");
        let _ = std::fs::remove_dir_all(rep.backup.unwrap());
        // a second scan finds nothing
        assert!(scan(&subject2, &Progress::default()).is_empty());
    }

    #[test]
    fn protected_paths_are_refused() {
        let t = Trace { item: Item::Path(PathBuf::from(r"C:\Windows")), label: "Windows".into(), kind: "Папка", reason: String::new(), confidence: Confidence::High, size: 0 };
        let rep = clean(&[t], "x", &Progress::default());
        assert_eq!(rep.removed, 0);
        assert_eq!(rep.failed.len(), 1);
        assert!(Path::new(r"C:\Windows").exists());
    }
}

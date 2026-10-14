//! Startup manager engine: everything that starts by itself — Run / RunOnce keys, Startup folders, auto-start
//! services and Task Scheduler tasks with a logon / boot trigger — plus a read-only look at the sneaky spots
//! (Winlogon, AppInit_DLLs, Image File Execution Options debuggers). Enabling and disabling follows what Task Manager
//! does (the `StartupApproved` flags), so the two always agree.

use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};

use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, DeleteService, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, OpenSCManagerW,
    OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW, SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE,
    SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
    SERVICE_DESCRIPTIONW, SERVICE_DISABLED, SERVICE_ERROR_NORMAL, SERVICE_QUERY_CONFIG, SERVICE_START, SERVICE_STATUS, SERVICE_STOP, SERVICE_WIN32, StartServiceW, SERVICE_STATE_ALL,
};

use crate::registry::{Hive, KeyPath, RegKey, View};
use crate::winapi::wide_str;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_ONCE: &str = r"Software\Microsoft\Windows\CurrentVersion\RunOnce";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
const DELETE: u32 = 0x0001_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Registry,
    Folder,
    Service,
    Task,
    Sneaky,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::Registry, Group::Folder, Group::Service, Group::Task, Group::Sneaky];

    pub fn title(self) -> &'static str {
        match self {
            Group::Registry => "Реестр (Run)",
            Group::Folder => "Папки автозагрузки",
            Group::Service => "Службы",
            Group::Task => "Планировщик задач",
            Group::Sneaky => "Скрытые точки входа",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartType {
    Auto,
    AutoDelayed,
    Manual,
    Disabled,
    /// Boot / system drivers and anything else we do not change.
    Other(u32),
}

impl StartType {
    pub fn title(self) -> &'static str {
        match self {
            StartType::Auto => "Автоматически",
            StartType::AutoDelayed => "Автоматически (отложенный запуск)",
            StartType::Manual => "Вручную",
            StartType::Disabled => "Отключена",
            StartType::Other(_) => "Системная",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Kind {
    /// A value in a Run / RunOnce key. `approved` is the StartupApproved key that holds its on/off flag.
    RunValue { key: KeyPath, value: String, approved: Option<KeyPath> },
    /// A file in a Startup folder.
    FolderItem { path: PathBuf, approved: KeyPath },
    Service { name: String, start: StartType, running: bool, account: String },
    /// `name` is the task path as `schtasks` wants it: `\Folder\Task`.
    Task { name: String, file: PathBuf },
    /// Read-only: shown with a link to regedit.
    Sneaky { key: KeyPath, value: String },
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub group: Group,
    pub kind: Kind,
    pub name: String,
    pub command: String,
    pub path: Option<PathBuf>,
    pub enabled: bool,
    /// Where it comes from, for display: `HKCU\...\Run`, a folder, the service name…
    pub origin: String,
    pub exists: bool,
    pub description: Option<String>,
    pub company: Option<String>,
    /// Windows' own entries (Microsoft-signed services, tasks below `\Microsoft\Windows`) — hidden by default.
    pub system: bool,
    /// Something looks off (file missing, odd location) — the reason.
    pub warning: Option<String>,
}

impl Entry {
    pub fn is_microsoft(&self) -> bool {
        self.company.as_deref().is_some_and(|c| c.to_lowercase().contains("microsoft"))
    }
}

// ---------------------------------------------------------------------------------------------------------
// Commands -> executables

/// The executable named by a command line: `"C:\a b\x.exe" /s`, `C:\a b\x.exe /s` (spaces without quotes), `x.exe`,
/// `rundll32 foo.dll,Entry`. Environment variables are expanded; a bare name is searched in `System32` and `PATH`.
pub fn exe_of_command(cmd: &str) -> Option<PathBuf> {
    let c = crate::registry::expand_env(cmd.trim());
    if c.is_empty() {
        return None;
    }
    if let Some(rest) = c.strip_prefix('"') {
        let end = rest.find('"')?;
        return resolve(&rest[..end]);
    }
    // longest prefix of space-separated words that is an existing file
    let words: Vec<&str> = c.split(' ').collect();
    for n in (1..=words.len()).rev() {
        let cand = words[..n].join(" ");
        if let Some(p) = resolve(&cand) {
            return Some(p);
        }
    }
    resolve(words[0])
}

fn resolve(s: &str) -> Option<PathBuf> {
    let s = s.trim().trim_matches('"');
    let p = Path::new(s);
    if p.is_absolute() {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        return None;
    }
    let with_ext = |base: PathBuf| -> Option<PathBuf> {
        if base.is_file() {
            return Some(base);
        }
        if base.extension().is_none() {
            let e = base.with_extension("exe");
            if e.is_file() {
                return Some(e);
            }
        }
        None
    };
    let sys = std::env::var_os("SystemRoot").map(PathBuf::from)?;
    with_ext(sys.join("System32").join(s)).or_else(|| with_ext(sys.join(s))).or_else(|| {
        std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).find_map(|d| with_ext(d.join(s))))
    })
}

fn finish(mut e: Entry) -> Entry {
    if e.path.is_none() {
        e.path = exe_of_command(&e.command);
    }
    e.exists = e.path.is_some();
    if let Some(p) = &e.path {
        let (d, c) = crate::procs::version_strings(p);
        e.description = d;
        e.company = c;
    }
    if !e.exists && e.group != Group::Sneaky && !e.command.trim().is_empty() {
        e.warning = Some("файл не найден — запись «битая»".into());
    } else if let Some(p) = &e.path {
        let n = crate::guard::normalize(p);
        let tmp = std::env::var("TEMP").map(|t| crate::guard::normalize(Path::new(&t))).unwrap_or_default();
        if (!tmp.is_empty() && n.starts_with(&tmp)) || n.contains(r"\appdata\local\temp\") || n.contains(r"\downloads\") {
            e.warning = Some("запускается из временной папки или «Загрузок»".into());
        }
    }
    e
}

// ---------------------------------------------------------------------------------------------------------
// StartupApproved

fn approved_enabled(key: &KeyPath, name: &str) -> bool {
    key.open(false).and_then(|k| k.get(name)).is_none_or(|v| v.data.first().is_none_or(|b| b & 1 == 0))
}

fn set_approved(key: &KeyPath, name: &str, enabled: bool) -> io::Result<()> {
    let k = RegKey::create(key.hive, key.view, &key.path)?;
    let mut data = [0u8; 12];
    if enabled {
        data[0] = 2;
    } else {
        data[0] = 3;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let ft = (now + 11_644_473_600) * 10_000_000;
        data[4..12].copy_from_slice(&ft.to_le_bytes());
    }
    k.set_binary(name, &data)
}

// ---------------------------------------------------------------------------------------------------------
// Listing

pub fn list() -> Vec<Entry> {
    let mut out = Vec::new();
    run_keys(&mut out);
    folders(&mut out);
    out.extend(list_services());
    tasks(&mut out);
    sneaky(&mut out);
    out
}

fn run_keys(out: &mut Vec<Entry>) {
    // (hive, view, key, once?, approved-subkey)
    let spots: [(Hive, View, &str, bool, Option<&str>); 6] = [
        (Hive::User, View::Native, RUN, false, Some("Run")),
        (Hive::Machine, View::Native, RUN, false, Some("Run")),
        (Hive::Machine, View::Wow32, RUN, false, Some("Run32")),
        (Hive::User, View::Native, RUN_ONCE, true, None),
        (Hive::Machine, View::Native, RUN_ONCE, true, None),
        (Hive::Machine, View::Wow32, RUN_ONCE, true, None),
    ];
    for (hive, view, path, once, appr) in spots {
        let kp = KeyPath::new(hive, view, path);
        let Some(key) = kp.open(false) else { continue };
        let approved = appr.map(|a| KeyPath::new(hive, View::Native, format!("{APPROVED}\\{a}")));
        for v in key.values() {
            let Some(cmd) = v.as_string() else { continue };
            let name = if v.name.is_empty() { "(по умолчанию)".to_string() } else { v.name.clone() };
            let enabled = approved.as_ref().is_none_or(|a| approved_enabled(a, &v.name));
            out.push(finish(Entry {
                group: Group::Registry,
                kind: Kind::RunValue { key: kp.clone(), value: v.name.clone(), approved: approved.clone() },
                name,
                command: cmd,
                path: None,
                enabled,
                origin: format!("{}{}", kp.display(), if once { "  (один раз)" } else { "" }),
                exists: false,
                description: None,
                company: None,
                system: false,
                warning: None,
            }));
        }
    }
}

fn startup_dirs() -> Vec<(PathBuf, Hive)> {
    let mut v = Vec::new();
    if let Some(a) = std::env::var_os("APPDATA") {
        v.push((PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\Startup"), Hive::User));
    }
    if let Some(a) = std::env::var_os("ProgramData") {
        v.push((PathBuf::from(a).join(r"Microsoft\Windows\Start Menu\Programs\StartUp"), Hive::Machine));
    }
    v
}

fn folders(out: &mut Vec<Entry>) {
    for (dir, hive) in startup_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let approved = KeyPath::new(hive, View::Native, format!("{APPROVED}\\StartupFolder"));
        for f in rd.flatten() {
            let fname = f.file_name().to_string_lossy().into_owned();
            if fname.eq_ignore_ascii_case("desktop.ini") || !f.path().is_file() {
                continue;
            }
            let (target, args) = if fname.to_lowercase().ends_with(".lnk") { lnk_target(&f.path()).unwrap_or_default() } else { (None, String::new()) };
            let command = match &target {
                Some(t) => format!("{} {}", t.display(), args).trim().to_string(),
                None => f.path().to_string_lossy().into_owned(),
            };
            let enabled = approved_enabled(&approved, &fname);
            out.push(finish(Entry {
                group: Group::Folder,
                kind: Kind::FolderItem { path: f.path(), approved: approved.clone() },
                name: fname,
                command,
                path: target.or_else(|| f.path().is_file().then(|| f.path())),
                enabled,
                origin: dir.to_string_lossy().into_owned(),
                exists: false,
                description: None,
                company: None,
                system: false,
                warning: None,
            }));
        }
    }
}

/// Target and arguments of a `.lnk` shortcut.
pub fn lnk_target(p: &Path) -> Option<(Option<PathBuf>, String)> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{Interface, PCWSTR};
    let w = wide_str(&p.to_string_lossy());
    // SAFETY: standard COM use on this thread; S_FALSE / RPC_E_CHANGED_MODE just mean COM is already initialised.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(PCWSTR(w.as_ptr()), STGM_READ).ok()?;
        let mut path = [0u16; 1024];
        let mut args = [0u16; 2048];
        let _ = link.GetPath(&mut path, std::ptr::null_mut(), 0);
        let _ = link.GetArguments(&mut args);
        let t = String::from_utf16_lossy(&path).trim_end_matches('\0').to_string();
        let a = String::from_utf16_lossy(&args).trim_end_matches('\0').to_string();
        Some(((!t.is_empty()).then(|| PathBuf::from(t)), a))
    }
}

// ---------------------------------------------------------------------------------------------------------
// Services

struct Scm(SC_HANDLE);

impl Drop for Scm {
    fn drop(&mut self) {
        // SAFETY: closed once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn open_scm(access: u32) -> io::Result<Scm> {
    // SAFETY: null machine / database names mean the local active database.
    let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    if h.is_null() { Err(io::Error::last_os_error()) } else { Ok(Scm(h)) }
}

fn open_service(scm: &Scm, name: &str, access: u32) -> io::Result<Scm> {
    let n = wide_str(name);
    // SAFETY: NUL-terminated name.
    let h = unsafe { OpenServiceW(scm.0, n.as_ptr(), access) };
    if h.is_null() { Err(io::Error::last_os_error()) } else { Ok(Scm(h)) }
}

fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    // SAFETY: the SCM returns NUL-terminated strings inside the buffer it filled.
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// Every Win32 service (not only auto-start ones).
pub fn list_services() -> Vec<Entry> {
    let mut out = Vec::new();
    services(&mut out);
    out
}

fn services(out: &mut Vec<Entry>) {
    let Ok(scm) = open_scm(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE) else { return };
    let mut buf = vec![0u64; 1 << 15];
    let mut resume = 0u32;
    loop {
        let (mut need, mut count) = (0u32, 0u32);
        // SAFETY: the buffer holds `buf.len() * 8` bytes; the out-parameters are live locals.
        let ok = unsafe {
            EnumServicesStatusExW(scm.0, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL, buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut need, &mut count, &mut resume, std::ptr::null())
        };
        let more = ok == 0 && io::Error::last_os_error().raw_os_error() == Some(234);
        if ok == 0 && !more {
            break;
        }
        // SAFETY: on success the buffer starts with `count` ENUM_SERVICE_STATUS_PROCESSW records.
        let recs = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize) };
        for r in recs {
            let name = pwstr(r.lpServiceName);
            let display = pwstr(r.lpDisplayName);
            let running = r.ServiceStatusProcess.dwCurrentState == 4;
            let Ok(svc) = open_service(&scm, &name, SERVICE_QUERY_CONFIG) else { continue };
            let Some((start, image, account, description)) = service_config(&svc) else { continue };
            let command = image.clone();
            let mut e = finish(Entry {
                group: Group::Service,
                kind: Kind::Service { name: name.clone(), start, running, account },
                name: if display.is_empty() { name.clone() } else { display },
                command,
                path: None,
                enabled: matches!(start, StartType::Auto | StartType::AutoDelayed | StartType::Other(_)),
                origin: format!("служба {name}"),
                exists: false,
                description: None,
                company: None,
                system: false,
                warning: None,
            });
            if description.is_some() {
                e.description = description;
            }
            e.system = e.is_microsoft();
            out.push(e);
        }
        if !more {
            break;
        }
    }
}

fn service_config(svc: &Scm) -> Option<(StartType, String, String, Option<String>)> {
    let mut need = 0u32;
    // SAFETY: the first call only asks for the size.
    unsafe { QueryServiceConfigW(svc.0, std::ptr::null_mut(), 0, &mut need) };
    if need == 0 {
        return None;
    }
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    // SAFETY: the buffer is at least `need` bytes and aligned.
    if unsafe { QueryServiceConfigW(svc.0, buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW, need, &mut need) } == 0 {
        return None;
    }
    // SAFETY: a successful call wrote a QUERY_SERVICE_CONFIGW whose strings point into the same buffer.
    let c = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
    let mut start = match c.dwStartType {
        SERVICE_AUTO_START => StartType::Auto,
        SERVICE_DEMAND_START => StartType::Manual,
        SERVICE_DISABLED => StartType::Disabled,
        other => StartType::Other(other),
    };
    if start == StartType::Auto {
        let mut b = [0u64; 2];
        let mut n = 0u32;
        // SAFETY: SERVICE_DELAYED_AUTO_START_INFO is one BOOL; the buffer is large enough.
        if unsafe { QueryServiceConfig2W(svc.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, b.as_mut_ptr() as *mut u8, 16, &mut n) } != 0 && b[0] as u32 != 0 {
            start = StartType::AutoDelayed;
        }
    }
    let description = {
        let mut n = 0u32;
        // SAFETY: size probe, then a buffer of that size.
        unsafe { QueryServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, std::ptr::null_mut(), 0, &mut n) };
        if n == 0 {
            None
        } else {
            let mut b = vec![0u64; (n as usize).div_ceil(8)];
            // SAFETY: as above; the description string points into `b`.
            if unsafe { QueryServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, b.as_mut_ptr() as *mut u8, n, &mut n) } != 0 {
                let d = unsafe { &*(b.as_ptr() as *const SERVICE_DESCRIPTIONW) };
                let s = pwstr(d.lpDescription);
                (!s.is_empty() && !s.starts_with('@')).then_some(s)
            } else {
                None
            }
        }
    };
    Some((start, pwstr(c.lpBinaryPathName), pwstr(c.lpServiceStartName), description))
}

/// Changes a service's start type (needs administrator rights).
pub fn set_service_start(name: &str, start: StartType) -> io::Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, SERVICE_CHANGE_CONFIG)?;
    let t = match start {
        StartType::Auto | StartType::AutoDelayed => SERVICE_AUTO_START,
        StartType::Manual => SERVICE_DEMAND_START,
        StartType::Disabled => SERVICE_DISABLED,
        StartType::Other(_) => return Err(io::Error::new(io::ErrorKind::InvalidInput, "тип запуска не меняется")),
    };
    // SAFETY: SERVICE_NO_CHANGE (u32::MAX) leaves the other settings alone; null strings mean "unchanged".
    let ok = unsafe { ChangeServiceConfigW(svc.0, u32::MAX, t, u32::MAX, std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let delayed: u32 = (start == StartType::AutoDelayed) as u32;
    if matches!(start, StartType::Auto | StartType::AutoDelayed) {
        // SAFETY: SERVICE_DELAYED_AUTO_START_INFO is a single BOOL.
        unsafe { ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, &delayed as *const u32 as *const c_void) };
    }
    Ok(())
}

pub fn start_service(name: &str) -> io::Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, SERVICE_START)?;
    // SAFETY: no arguments.
    if unsafe { StartServiceW(svc.0, 0, std::ptr::null()) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

pub fn stop_service(name: &str) -> io::Result<()> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, SERVICE_STOP)?;
    // SAFETY: SERVICE_STATUS is plain data.
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Stops and removes a service from the SCM.
pub fn delete_service(name: &str) -> io::Result<()> {
    let _ = stop_service(name);
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    let svc = open_service(&scm, name, DELETE)?;
    // SAFETY: valid handle with DELETE access.
    if unsafe { DeleteService(svc.0) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The image path of one service (`None` if it does not exist).
pub fn service_image(name: &str) -> Option<String> {
    let scm = open_scm(SC_MANAGER_CONNECT).ok()?;
    let svc = open_service(&scm, name, SERVICE_QUERY_CONFIG).ok()?;
    service_config(&svc).map(|c| c.1)
}

#[allow(dead_code)]
const _: u32 = SERVICE_ERROR_NORMAL;

// ---------------------------------------------------------------------------------------------------------
// Scheduled tasks

fn tasks_dir() -> Option<PathBuf> {
    std::env::var_os("SystemRoot").map(|s| PathBuf::from(s).join(r"System32\Tasks"))
}

fn read_xml(p: &Path) -> Option<String> {
    let b = std::fs::read(p).ok()?;
    if b.starts_with(&[0xFF, 0xFE]) {
        let w: Vec<u16> = b[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        Some(String::from_utf16_lossy(&w))
    } else if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some(String::from_utf8_lossy(&b[3..]).into_owned())
    } else {
        Some(String::from_utf8_lossy(&b).into_owned())
    }
}

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find(&format!("</{tag}>"))? + s;
    Some(xml[s..e].trim().replace("&amp;", "&").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&apos;", "'"))
}

/// What a task XML says: (has logon/boot trigger, enabled, command, arguments, description).
fn parse_task(xml: &str) -> (bool, bool, String, String, Option<String>) {
    let triggers = xml.contains("<LogonTrigger") || xml.contains("<BootTrigger");
    let settings = xml.find("<Settings>").map(|i| &xml[i..]).unwrap_or("");
    let enabled = xml_tag(settings, "Enabled").is_none_or(|v| !v.eq_ignore_ascii_case("false"));
    let exec = xml.find("<Exec>").map(|i| &xml[i..]).unwrap_or("");
    let command = xml_tag(exec, "Command").unwrap_or_default();
    let args = xml_tag(exec, "Arguments").unwrap_or_default();
    (triggers, enabled, command, args, xml_tag(xml, "Description"))
}

fn tasks(out: &mut Vec<Entry>) {
    let Some(root) = tasks_dir() else { return };
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for f in rd.flatten() {
            let p = f.path();
            let Ok(ft) = f.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            let Some(xml) = read_xml(&p) else { continue };
            let (trig, enabled, cmd, args, desc) = parse_task(&xml);
            if !trig {
                continue;
            }
            let rel = p.strip_prefix(&root).unwrap_or(&p).to_string_lossy().into_owned();
            let name = format!("\\{rel}");
            let mut e = finish(Entry {
                group: Group::Task,
                kind: Kind::Task { name: name.clone(), file: p.clone() },
                name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                command: format!("{cmd} {args}").trim().to_string(),
                path: None,
                enabled,
                origin: format!("задача {name}"),
                exists: false,
                description: None,
                company: None,
                system: rel.to_lowercase().starts_with(r"microsoft\windows\"),
                warning: None,
            });
            if desc.as_deref().is_some_and(|d| !d.starts_with('@')) {
                e.description = desc;
            }
            out.push(e);
        }
    }
}

fn schtasks(args: &[&str]) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let o = std::process::Command::new("schtasks.exe").args(args).creation_flags(0x0800_0000).output()?;
    if o.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
        let msg = if msg.is_empty() { String::from_utf8_lossy(&o.stdout).trim().to_string() } else { msg };
        Err(io::Error::other(if msg.is_empty() { "schtasks не выполнился".into() } else { msg }))
    }
}

pub fn delete_task(name: &str) -> io::Result<()> {
    schtasks(&["/Delete", "/TN", name, "/F"])
}

// ---------------------------------------------------------------------------------------------------------
// Sneaky spots (read-only)

fn sneaky(out: &mut Vec<Entry>) {
    let mut push = |key: KeyPath, value: &str, cmd: String, what: &str, warn: Option<String>| {
        let mut e = finish(Entry {
            group: Group::Sneaky,
            kind: Kind::Sneaky { key: key.clone(), value: value.to_string() },
            name: what.to_string(),
            command: cmd,
            path: None,
            enabled: true,
            origin: key.display(),
            exists: false,
            description: None,
            company: None,
            system: false,
            warning: None,
        });
        e.warning = warn.or(e.warning);
        out.push(e);
    };
    let winlogon = KeyPath::new(Hive::Machine, View::Native, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon");
    if let Some(k) = winlogon.open(false) {
        for (v, normal) in [("Shell", "explorer.exe"), ("Userinit", r"c:\windows\system32\userinit.exe,"), ("Taskman", "")] {
            if let Some(s) = k.get_string(v)
                && !s.is_empty()
            {
                let ok = s.to_lowercase().trim_end_matches(',') == normal.trim_end_matches(',');
                push(winlogon.clone(), v, s, &format!("Winlogon: {v}"), (!ok).then(|| "нестандартное значение — так делают вредоносные программы".into()));
            }
        }
    }
    for view in [View::Native, View::Wow32] {
        let win = KeyPath::new(Hive::Machine, view, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Windows");
        if let Some(k) = win.open(false)
            && let Some(s) = k.get_string("AppInit_DLLs")
            && !s.trim().is_empty()
        {
            push(win.clone(), "AppInit_DLLs", s, "AppInit_DLLs (DLL во всех программах)", Some("загружается в каждый процесс с интерфейсом".into()));
        }
    }
    for view in [View::Native, View::Wow32] {
        let ifeo = KeyPath::new(Hive::Machine, view, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options");
        let Some(k) = ifeo.open(false) else { continue };
        for sub in k.subkeys() {
            let sk = ifeo.child(&sub);
            if let Some(d) = sk.open(false).and_then(|x| x.get_string("Debugger"))
                && !d.trim().is_empty()
            {
                push(sk, "Debugger", d, &format!("Подмена запуска {sub}"), Some("вместо программы запускается другая (отладчик)".into()));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Actions

/// Turns an entry on or off the way Task Manager does. Needs administrator rights for machine-wide entries.
pub fn set_enabled(e: &Entry, on: bool) -> io::Result<()> {
    match &e.kind {
        Kind::RunValue { value, approved, .. } => match approved {
            Some(a) => set_approved(a, value, on),
            None => Err(io::Error::new(io::ErrorKind::Unsupported, "записи RunOnce нельзя отключить — только удалить")),
        },
        Kind::FolderItem { path, approved } => set_approved(approved, &path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), on),
        Kind::Service { name, .. } => set_service_start(name, if on { StartType::Auto } else { StartType::Disabled }),
        Kind::Task { name, .. } => schtasks(&["/Change", "/TN", name, if on { "/ENABLE" } else { "/DISABLE" }]),
        Kind::Sneaky { .. } => Err(io::Error::new(io::ErrorKind::Unsupported, "эту запись можно только открыть в редакторе реестра")),
    }
}

/// Removes an entry for good, after saving a copy of it in `backup_dir`.
pub fn remove(e: &Entry, backup_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(backup_dir)?;
    let stamp: String = e.name.chars().filter(|c| c.is_alphanumeric()).collect();
    match &e.kind {
        Kind::RunValue { key, value, approved } => {
            let _ = crate::registry::export(key, &backup_dir.join(format!("run-{stamp}.reg")));
            let k = key.open(true).ok_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied))?;
            k.delete_value(value)?;
            if let Some(a) = approved
                && let Some(ak) = a.open(true)
            {
                let _ = ak.delete_value(value);
            }
            Ok(())
        }
        Kind::FolderItem { path, approved } => {
            if let Some(n) = path.file_name() {
                let _ = std::fs::copy(path, backup_dir.join(n));
                if let Some(ak) = approved.open(true) {
                    let _ = ak.delete_value(&n.to_string_lossy());
                }
            }
            std::fs::remove_file(path)
        }
        Kind::Service { name, .. } => {
            let key = KeyPath::new(Hive::Machine, View::Native, format!(r"SYSTEM\CurrentControlSet\Services\{name}"));
            let _ = crate::registry::export(&key, &backup_dir.join(format!("service-{stamp}.reg")));
            delete_service(name)
        }
        Kind::Task { name, file } => {
            let _ = std::fs::copy(file, backup_dir.join(format!("task-{stamp}.xml")));
            delete_task(name)
        }
        Kind::Sneaky { .. } => Err(io::Error::new(io::ErrorKind::Unsupported, "эту запись нужно править вручную в редакторе реестра")),
    }
}

/// Opens regedit on a key (it remembers the last key in `HKCU\...\Applets\Regedit\LastKey`).
pub fn open_in_regedit(k: &KeyPath) {
    let hive = match k.hive {
        Hive::Machine => "HKEY_LOCAL_MACHINE",
        Hive::User => "HKEY_CURRENT_USER",
    };
    if let Ok(rk) = RegKey::create(Hive::User, View::Native, r"Software\Microsoft\Windows\CurrentVersion\Applets\Regedit") {
        let v: Vec<u8> = format!("Компьютер\\{hive}\\{}", k.path).encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect();
        let _ = rk.set_raw("LastKey", 1, &v);
    }
    let _ = std::process::Command::new("regedit.exe").spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parsing() {
        let sys = std::env::var("SystemRoot").unwrap();
        let cmd = format!(r#""{sys}\System32\notepad.exe" /A"#);
        assert_eq!(exe_of_command(&cmd).unwrap(), PathBuf::from(format!(r"{sys}\System32\notepad.exe")));
        assert!(exe_of_command(&format!(r"{sys}\System32\notepad.exe /A")).is_some(), "unquoted with arguments");
        assert!(exe_of_command("%SystemRoot%\\System32\\cmd.exe /c x").is_some(), "env vars");
        assert!(exe_of_command("notepad.exe").is_some(), "bare name from System32");
        assert!(exe_of_command(r"C:\no\such\file.exe").is_none());
        assert!(exe_of_command("").is_none());
    }

    #[test]
    fn task_xml() {
        let xml = r#"<Task><RegistrationInfo><Description>Updates &amp; things</Description></RegistrationInfo>
            <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>
            <Settings><Enabled>false</Enabled></Settings>
            <Actions><Exec><Command>"C:\x y\z.exe"</Command><Arguments>/q</Arguments></Exec></Actions></Task>"#;
        let (t, en, c, a, d) = parse_task(xml);
        assert!(t && !en);
        assert_eq!((c.as_str(), a.as_str()), (r#""C:\x y\z.exe""#, "/q"));
        assert_eq!(d.as_deref(), Some("Updates & things"));
        assert!(!parse_task("<Task><Triggers><TimeTrigger/></Triggers></Task>").0);
    }

    #[test]
    fn approved_flag_roundtrip() {
        let k = KeyPath::new(Hive::User, View::Native, format!("Software\\StrelaTestStartup-{}\\Approved", std::process::id()));
        assert!(approved_enabled(&k, "x"), "no value means enabled");
        set_approved(&k, "x", false).unwrap();
        assert!(!approved_enabled(&k, "x"));
        set_approved(&k, "x", true).unwrap();
        assert!(approved_enabled(&k, "x"));
        let top = KeyPath::new(Hive::User, View::Native, format!("Software\\StrelaTestStartup-{}", std::process::id()));
        crate::registry::delete_tree(&top).unwrap();
    }

    #[test]
    fn lists_something_on_a_real_machine() {
        let all = list();
        assert!(all.iter().any(|e| e.group == Group::Service), "services");
        assert!(all.iter().any(|e| e.group == Group::Service && e.system), "windows services are marked");
        // every entry carries a name and an origin
        assert!(all.iter().all(|e| !e.name.is_empty() && !e.origin.is_empty()));
    }
}

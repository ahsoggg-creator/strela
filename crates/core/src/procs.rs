//! Process manager engine: a fast snapshot with CPU / memory / I/O figures, command lines and modules on demand,
//! and the actions (terminate, terminate tree, priority, suspend / resume).

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{GetTokenInformation, LookupAccountSidW, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
    TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetPriorityClass, GetProcessHandleCount, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS, IsProcessCritical, IsWow64Process, OpenProcess,
    OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    SetPriorityClass, TerminateProcess,
};

use crate::winapi::{OwnedH, wide_str};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
    Realtime,
}

impl Priority {
    pub const ALL: [Priority; 6] = [Priority::Idle, Priority::BelowNormal, Priority::Normal, Priority::AboveNormal, Priority::High, Priority::Realtime];

    fn class(self) -> u32 {
        match self {
            Priority::Idle => 0x40,
            Priority::BelowNormal => 0x4000,
            Priority::Normal => 0x20,
            Priority::AboveNormal => 0x8000,
            Priority::High => 0x80,
            Priority::Realtime => 0x100,
        }
    }

    fn from_class(c: u32) -> Option<Priority> {
        Priority::ALL.into_iter().find(|p| p.class() == c)
    }

    pub fn title(self) -> &'static str {
        match self {
            Priority::Idle => "Низкий",
            Priority::BelowNormal => "Ниже обычного",
            Priority::Normal => "Обычный",
            Priority::AboveNormal => "Выше обычного",
            Priority::High => "Высокий",
            Priority::Realtime => "Реального времени",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub path: Option<PathBuf>,
    pub threads: u32,
    /// `false` when Windows refused to open the process (system / protected processes, other users when not elevated).
    pub accessible: bool,
    pub cpu: f32,
    pub cpu_seconds: f64,
    pub working_set: u64,
    pub private_bytes: u64,
    pub peak_working_set: u64,
    pub handles: u32,
    pub io_read: u64,
    pub io_write: u64,
    pub start_unix: Option<u64>,
    pub user: Option<String>,
    pub priority: Option<Priority>,
    pub wow64: bool,
    pub critical: bool,
    pub description: Option<String>,
    pub company: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ModuleInfo {
    pub name: String,
    pub path: PathBuf,
    pub base: usize,
    pub size: u32,
}

fn open(pid: u32, access: u32) -> Option<OwnedH> {
    // SAFETY: plain OpenProcess; a null result means failure.
    let h = unsafe { OpenProcess(access, 0, pid) };
    (!h.is_null()).then_some(OwnedH(h))
}

fn ft(f: &FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

fn from_wide_nul(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn image_path(h: HANDLE) -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer holds `len` characters.
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

fn token_user(h: HANDLE) -> Option<String> {
    let mut tok: HANDLE = std::ptr::null_mut();
    // SAFETY: out-pointer references a live local.
    if unsafe { OpenProcessToken(h, TOKEN_QUERY, &mut tok) } == 0 {
        return None;
    }
    let tok = OwnedH(tok);
    let mut need = 0u32;
    // SAFETY: the first call only asks for the size.
    unsafe { GetTokenInformation(tok.0, TokenUser, std::ptr::null_mut(), 0, &mut need) };
    if need == 0 {
        return None;
    }
    let mut buf = vec![0u64; (need as usize).div_ceil(8)];
    // SAFETY: the buffer is at least `need` bytes and 8-byte aligned.
    if unsafe { GetTokenInformation(tok.0, TokenUser, buf.as_mut_ptr() as *mut c_void, need, &mut need) } == 0 {
        return None;
    }
    // SAFETY: a successful TokenUser query wrote a TOKEN_USER at the start of the buffer.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let (mut nl, mut dl) = (256u32, 256u32);
    let (mut name, mut dom) = (vec![0u16; 256], vec![0u16; 256]);
    let mut kind = 0i32;
    // SAFETY: both buffers hold the lengths passed.
    let ok = unsafe { LookupAccountSidW(std::ptr::null(), sid, name.as_mut_ptr(), &mut nl, dom.as_mut_ptr(), &mut dl, &mut kind) };
    if ok == 0 {
        return None;
    }
    let (n, d) = (String::from_utf16_lossy(&name[..nl as usize]), String::from_utf16_lossy(&dom[..dl as usize]));
    Some(if d.is_empty() { n } else { format!("{d}\\{n}") })
}

/// File description and company from the version resource of an executable.
pub fn version_strings(path: &std::path::Path) -> (Option<String>, Option<String>) {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let p = crate::winapi::wide(path);
    // SAFETY: `p` is NUL-terminated; the handle out-parameter is optional.
    let size = unsafe { GetFileVersionInfoSizeW(p.as_ptr(), std::ptr::null_mut()) };
    if size == 0 {
        return (None, None);
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: the buffer holds `size` bytes.
    if unsafe { GetFileVersionInfoW(p.as_ptr(), 0, size, data.as_mut_ptr() as *mut c_void) } == 0 {
        return (None, None);
    }
    let query = |sub: &str| -> Option<String> {
        let q = wide_str(sub);
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: `data` is a valid version block; on success `ptr` points into it with `len` characters.
        let ok = unsafe { VerQueryValueW(data.as_ptr() as *const c_void, q.as_ptr(), &mut ptr, &mut len) };
        if ok == 0 || ptr.is_null() || len == 0 {
            return None;
        }
        // SAFETY: see above; the slice stays inside `data`.
        let s = unsafe { std::slice::from_raw_parts(ptr as *const u16, len as usize) };
        let t = from_wide_nul(s).trim().to_string();
        (!t.is_empty()).then_some(t)
    };
    // the translation table: language and code page of the first block
    let mut lang = String::from("040904B0");
    {
        let q = wide_str("\\VarFileInfo\\Translation");
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: as above; the translation is a pair of u16.
        if unsafe { VerQueryValueW(data.as_ptr() as *const c_void, q.as_ptr(), &mut ptr, &mut len) } != 0 && len >= 4 && !ptr.is_null() {
            let t = unsafe { std::slice::from_raw_parts(ptr as *const u16, 2) };
            lang = format!("{:04X}{:04X}", t[0], t[1]);
        }
    }
    let get = |k: &str| query(&format!("\\StringFileInfo\\{lang}\\{k}")).or_else(|| query(&format!("\\StringFileInfo\\040904B0\\{k}")));
    (get("FileDescription"), get("CompanyName"))
}

/// Keeps what is needed between snapshots to compute CPU usage.
pub struct Sampler {
    prev: HashMap<u32, (u64, u64)>, // pid -> (cpu 100ns ticks, creation filetime)
    prev_at: Option<Instant>,
    cpus: f64,
    versions: HashMap<PathBuf, (Option<String>, Option<String>)>,
    users: HashMap<u32, Option<String>>,
    me: u32,
}

impl Default for Sampler {
    fn default() -> Self {
        Sampler::new()
    }
}

impl Sampler {
    pub fn new() -> Sampler {
        // SAFETY: GetCurrentProcessId has no preconditions.
        let me = unsafe { GetCurrentProcessId() };
        Sampler { prev: HashMap::new(), prev_at: None, cpus: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64, versions: HashMap::new(), users: HashMap::new(), me }
    }

    pub fn own_pid(&self) -> u32 {
        self.me
    }

    pub fn snapshot(&mut self) -> io::Result<Vec<ProcInfo>> {
        // SAFETY: plain snapshot of the process list.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snap = OwnedH(snap);
        let now = Instant::now();
        let wall_ticks = self.prev_at.map(|t| now.duration_since(t).as_secs_f64() * 1e7);
        let mut next_prev = HashMap::with_capacity(self.prev.len());
        let mut out = Vec::with_capacity(256);
        // SAFETY: PROCESSENTRY32W is plain data; dwSize must be set before the first call.
        let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = unsafe { Process32FirstW(snap.0, &mut e) };
        while ok != 0 {
            let mut p = ProcInfo { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, name: from_wide_nul(&e.szExeFile), threads: e.cntThreads, ..Default::default() };
            if p.pid == 0 {
                p.name = "System Idle Process".into();
            }
            if let Some(h) = open(p.pid, PROCESS_QUERY_LIMITED_INFORMATION) {
                p.accessible = true;
                p.path = image_path(h.0);
                let (mut c, mut x, mut k, mut u) = (zero_ft(), zero_ft(), zero_ft(), zero_ft());
                // SAFETY: four live FILETIME out-parameters.
                if unsafe { GetProcessTimes(h.0, &mut c, &mut x, &mut k, &mut u) } != 0 {
                    let cpu_ticks = ft(&k) + ft(&u);
                    p.cpu_seconds = cpu_ticks as f64 / 1e7;
                    let created = ft(&c);
                    p.start_unix = created.checked_sub(116_444_736_000_000_000).map(|t| t / 10_000_000);
                    if let (Some(w), Some(&(prev_ticks, prev_created))) = (wall_ticks, self.prev.get(&p.pid))
                        && prev_created == created
                        && w > 0.0
                    {
                        p.cpu = (((cpu_ticks.saturating_sub(prev_ticks)) as f64 / (w * self.cpus)) * 100.0).clamp(0.0, 100.0) as f32;
                    }
                    next_prev.insert(p.pid, (cpu_ticks, created));
                }
                // SAFETY: the structs are plain data of the stated sizes.
                unsafe {
                    let mut m: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
                    m.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
                    if GetProcessMemoryInfo(h.0, &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb) != 0 {
                        p.working_set = m.WorkingSetSize as u64;
                        p.peak_working_set = m.PeakWorkingSetSize as u64;
                        p.private_bytes = m.PrivateUsage as u64;
                    }
                    let mut io: IO_COUNTERS = std::mem::zeroed();
                    if GetProcessIoCounters(h.0, &mut io) != 0 {
                        p.io_read = io.ReadTransferCount;
                        p.io_write = io.WriteTransferCount;
                    }
                    let mut hc = 0u32;
                    if GetProcessHandleCount(h.0, &mut hc) != 0 {
                        p.handles = hc;
                    }
                    p.priority = Priority::from_class(GetPriorityClass(h.0));
                    let mut w = 0i32;
                    if IsWow64Process(h.0, &mut w) != 0 {
                        p.wow64 = w != 0;
                    }
                    let mut crit = 0i32;
                    if IsProcessCritical(h.0, &mut crit) != 0 {
                        p.critical = crit != 0;
                    }
                }
                let user = self.users.entry(p.pid).or_insert_with(|| token_user(h.0));
                p.user = user.clone();
            }
            if let Some(path) = &p.path {
                let (d, c) = self.versions.entry(path.clone()).or_insert_with(|| version_strings(path)).clone();
                p.description = d;
                p.company = c;
            }
            out.push(p);
            ok = unsafe { Process32NextW(snap.0, &mut e) };
        }
        let alive: std::collections::HashSet<u32> = out.iter().map(|p| p.pid).collect();
        self.users.retain(|pid, _| alive.contains(pid));
        self.prev = next_prev;
        self.prev_at = Some(now);
        Ok(out)
    }
}

fn zero_ft() -> FILETIME {
    FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }
}

/// Loaded modules (DLLs) of a process.
pub fn modules(pid: u32) -> io::Result<Vec<ModuleInfo>> {
    // SAFETY: plain snapshot of one process's modules.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snap = OwnedH(snap);
    let mut out = Vec::new();
    // SAFETY: MODULEENTRY32W is plain data; dwSize is set before the first call.
    let mut e: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
    let mut ok = unsafe { Module32FirstW(snap.0, &mut e) };
    while ok != 0 {
        out.push(ModuleInfo { name: from_wide_nul(&e.szModule), path: PathBuf::from(from_wide_nul(&e.szExePath)), base: e.modBaseAddr as usize, size: e.modBaseSize });
        ok = unsafe { Module32NextW(snap.0, &mut e) };
    }
    Ok(out)
}

fn ntdll_fn(name: &[u8]) -> Option<*const c_void> {
    let dll = wide_str("ntdll.dll");
    // SAFETY: NUL-terminated names; ntdll is loaded in every process.
    unsafe {
        let m = GetModuleHandleW(dll.as_ptr());
        if m.is_null() {
            return None;
        }
        GetProcAddress(m, name.as_ptr()).map(|f| f as *const c_void)
    }
}

/// The command line of a process (Windows 8.1+; needs the same rights as `QueryFullProcessImageName`).
pub fn command_line(pid: u32) -> Option<String> {
    type NtQuery = unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;
    let f = ntdll_fn(b"NtQueryInformationProcess\0")?;
    // SAFETY: the exported function has exactly this signature.
    let nt: NtQuery = unsafe { std::mem::transmute(f) };
    let h = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
    let mut need = 0u32;
    // SAFETY: the first call with an empty buffer only reports the size.
    unsafe { nt(h.0, PROCESS_COMMAND_LINE_INFORMATION, std::ptr::null_mut(), 0, &mut need) };
    if need == 0 || need > 1 << 20 {
        return None;
    }
    let mut buf = vec![0u64; (need as usize).div_ceil(8) + 1];
    // SAFETY: the buffer is at least `need` bytes, 8-byte aligned.
    let st = unsafe { nt(h.0, PROCESS_COMMAND_LINE_INFORMATION, buf.as_mut_ptr() as *mut c_void, need, &mut need) };
    if st < 0 {
        return None;
    }
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        max: u16,
        buffer: *const u16,
    }
    // SAFETY: on success the buffer starts with a UNICODE_STRING whose text follows it inside the same buffer.
    let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    if us.buffer.is_null() || us.length == 0 {
        return None;
    }
    // SAFETY: `length` bytes at `buffer`, inside our buffer.
    let s = unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) };
    Some(String::from_utf16_lossy(s))
}

/// Why a process must not be terminated from here, if so (critical system processes, Strela itself).
pub fn protected_reason(p: &ProcInfo, own_pid: u32) -> Option<&'static str> {
    if p.pid == own_pid {
        return Some("это сама Strela");
    }
    if p.pid == 0 || p.pid == 4 {
        return Some("системный процесс");
    }
    if p.critical {
        return Some("критический процесс Windows: его завершение приведёт к синему экрану");
    }
    None
}

fn nt_proc_call(pid: u32, name: &[u8]) -> io::Result<()> {
    type NtProc = unsafe extern "system" fn(HANDLE) -> i32;
    let f = ntdll_fn(name).ok_or_else(|| io::Error::other("ntdll"))?;
    // SAFETY: NtSuspendProcess / NtResumeProcess take one handle and return an NTSTATUS.
    let nt: NtProc = unsafe { std::mem::transmute(f) };
    let h = open(pid, PROCESS_SUSPEND_RESUME).ok_or_else(io::Error::last_os_error)?;
    // SAFETY: valid process handle.
    let st = unsafe { nt(h.0) };
    if st >= 0 { Ok(()) } else { Err(io::Error::other(format!("NTSTATUS {st:#x}"))) }
}

pub fn suspend(pid: u32) -> io::Result<()> {
    nt_proc_call(pid, b"NtSuspendProcess\0")
}

pub fn resume(pid: u32) -> io::Result<()> {
    nt_proc_call(pid, b"NtResumeProcess\0")
}

pub fn set_priority(pid: u32, p: Priority) -> io::Result<()> {
    let h = open(pid, PROCESS_SET_INFORMATION).ok_or_else(io::Error::last_os_error)?;
    // SAFETY: valid handle with PROCESS_SET_INFORMATION.
    if unsafe { SetPriorityClass(h.0, p.class()) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Terminates one process (no questions asked: the caller decides what is allowed).
pub fn terminate(pid: u32) -> io::Result<()> {
    let h = open(pid, PROCESS_TERMINATE).ok_or_else(io::Error::last_os_error)?;
    // SAFETY: valid handle with PROCESS_TERMINATE.
    if unsafe { TerminateProcess(h.0, 1) } != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// `root` and all its descendants, children first.
pub fn tree_pids(root: u32, all: &[ProcInfo]) -> Vec<u32> {
    let mut order = Vec::new();
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        order.push(pid);
        // a pid can be reused: only count real children (started after the parent is irrelevant here; the snapshot is atomic)
        for c in all.iter().filter(|p| p.ppid == pid && p.pid != pid) {
            stack.push(c.pid);
        }
    }
    order.reverse();
    order
}

/// Terminates every process in `pids`, skipping `protected` ones. Returns (terminated, failures).
pub fn terminate_many(pids: &[u32], all: &[ProcInfo], own_pid: u32) -> (usize, Vec<(u32, String)>) {
    let mut done = 0;
    let mut failed = Vec::new();
    for &pid in pids {
        if let Some(p) = all.iter().find(|p| p.pid == pid)
            && let Some(why) = protected_reason(p, own_pid)
        {
            failed.push((pid, why.to_string()));
            continue;
        }
        match terminate(pid) {
            Ok(()) => done += 1,
            Err(e) => failed.push((pid, e.to_string())),
        }
    }
    (done, failed)
}

/// Processes whose executable lies inside `dir` (used before removing a program).
pub fn running_from(dir: &std::path::Path, all: &[ProcInfo]) -> Vec<u32> {
    let d = crate::guard::normalize(dir);
    all.iter()
        .filter(|p| {
            p.path.as_ref().is_some_and(|x| {
                let n = crate::guard::normalize(x);
                n.len() > d.len() && n.starts_with(&d) && n.as_bytes()[d.len()] == b'\\'
            })
        })
        .map(|p| p.pid)
        .collect()
}

#[allow(dead_code)]
fn _keep(_: HANDLE) {
    let _ = CloseHandle as unsafe extern "system" fn(HANDLE) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_sees_this_process() {
        let mut s = Sampler::new();
        let first = s.snapshot().unwrap();
        let me = first.iter().find(|p| p.pid == s.own_pid()).expect("own process");
        assert!(me.accessible);
        assert!(me.working_set > 0);
        assert!(me.path.is_some());
        assert!(me.threads >= 1);
        std::thread::sleep(std::time::Duration::from_millis(150));
        let second = s.snapshot().unwrap();
        assert!(second.len() > 10);
        assert!(second.iter().all(|p| (0.0..=100.0).contains(&p.cpu)));
    }

    #[test]
    fn command_line_and_modules_of_self() {
        let pid = std::process::id();
        let cl = command_line(pid).expect("command line");
        assert!(!cl.is_empty());
        let m = modules(pid).unwrap();
        assert!(m.iter().any(|m| m.name.eq_ignore_ascii_case("kernel32.dll")));
    }

    #[test]
    fn terminate_a_child_and_its_tree() {
        let mut child = std::process::Command::new("cmd").args(["/C", "ping -n 30 127.0.0.1 >NUL"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        let mut s = Sampler::new();
        let all = s.snapshot().unwrap();
        let tree = tree_pids(child.id(), &all);
        assert!(tree.contains(&child.id()));
        assert!(tree.len() >= 2, "cmd + ping: {tree:?}");
        assert_eq!(*tree.last().unwrap(), child.id(), "children first, root last");
        let (done, failed) = terminate_many(&tree, &all, s.own_pid());
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(done, tree.len());
        let st = child.wait().unwrap();
        assert!(!st.success());
    }

    #[test]
    fn critical_and_self_are_protected() {
        let mut s = Sampler::new();
        let all = s.snapshot().unwrap();
        let me = all.iter().find(|p| p.pid == s.own_pid()).unwrap();
        assert!(protected_reason(me, s.own_pid()).is_some());
        let sys = all.iter().find(|p| p.pid == 4).unwrap();
        assert!(protected_reason(sys, s.own_pid()).is_some());
    }

    #[test]
    fn version_strings_of_explorer() {
        let (d, c) = version_strings(std::path::Path::new(r"C:\Windows\explorer.exe"));
        assert!(d.is_some() || c.is_some());
    }
}

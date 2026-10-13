//! Local named-pipe IPC: length-prefixed JSON messages.
//!
//! Two users: the indexer service (`\\.\pipe\StrelaIndexer`, answers search requests) and the running GUI instance
//! (`\\.\pipe\Strela-<user>`, receives commands from the Explorer context menu so no second window is started).

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW};

use crate::index::Index;
use crate::indexer::{Indexer, Phase};
use crate::query::{Options, Query};
use crate::winapi::wide_str;

pub const SERVICE_PIPE: &str = r"\\.\pipe\StrelaIndexer";
/// Authenticated users may read and write; SYSTEM and administrators have full control.
pub const SERVICE_SDDL: &str = "D:(A;;GRGW;;;AU)(A;;GA;;;SY)(A;;GA;;;BA)";
/// Messages larger than this are refused (a bug or an attack, not a request).
pub const MAX_MESSAGE: usize = 32 << 20;

const PIPE_ACCESS_DUPLEX: u32 = 3;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_READMODE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 8;

/// `\\.\pipe\Strela-<user name>`
pub fn user_pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let clean: String = user.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    format!(r"\\.\pipe\Strela-{clean}")
}

pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_MESSAGE {
        return Err(io::Error::other("сообщение слишком большое"));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_msg<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "сообщение слишком большое"));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub struct PipeServer {
    name: Vec<u16>,
    plain_name: String,
    sddl: Option<Vec<u16>>,
    first: bool,
}

impl PipeServer {
    /// `sddl = None` keeps the default security descriptor (creator, SYSTEM and administrators only).
    pub fn new(name: &str, sddl: Option<&str>) -> PipeServer {
        PipeServer { name: wide_str(name), plain_name: name.to_string(), sddl: sddl.map(wide_str), first: true }
    }

    /// Blocks until a client connects; every call creates a fresh pipe instance.
    pub fn accept(&mut self) -> io::Result<File> {
        let mut psd: *mut c_void = std::ptr::null_mut();
        if let Some(s) = &self.sddl {
            // SAFETY: `s` is NUL-terminated; the out pointer references a live local.
            let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), 1, &mut psd, std::ptr::null_mut()) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd,
            bInheritHandle: 0,
        };
        let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        // SAFETY: the name is NUL-terminated; `sa` is only read during the call.
        let h = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                mode,
                255,
                64 * 1024,
                64 * 1024,
                0,
                if psd.is_null() { std::ptr::null() } else { &sa },
            )
        };
        let err = io::Error::last_os_error();
        if !psd.is_null() {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(psd) };
        }
        if h == INVALID_HANDLE_VALUE {
            return Err(err);
        }
        self.first = false;
        // SAFETY: `h` is a fresh valid pipe handle; ownership moves into the File (closed on drop).
        let file = unsafe { File::from_raw_handle(h) };
        use std::os::windows::io::AsRawHandle;
        // SAFETY: valid pipe handle, synchronous mode.
        let ok = unsafe { ConnectNamedPipe(file.as_raw_handle(), std::ptr::null_mut()) };
        if ok == 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                return Err(e);
            }
        }
        Ok(file)
    }

    /// Wakes a blocked `accept` (used on shutdown).
    pub fn poke(&self) {
        let _ = File::options().read(true).write(true).open(&self.plain_name);
    }
}

/// Ends a served conversation.
pub fn hang_up(f: File) {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: valid pipe handle; flushes pending data first so the client reads the reply.
    unsafe {
        windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(f.as_raw_handle());
        DisconnectNamedPipe(f.as_raw_handle());
    }
}

/// Opens the pipe, waiting briefly while every instance is busy.
pub fn connect(name: &str, wait: Duration) -> io::Result<File> {
    let w = wide_str(name);
    let deadline = std::time::Instant::now() + wait;
    loop {
        match File::options().read(true).write(true).open(name) {
            Ok(f) => return Ok(f),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && std::time::Instant::now() < deadline => {
                // SAFETY: NUL-terminated name.
                unsafe { WaitNamedPipeW(w.as_ptr(), 200) };
            }
            Err(e) => return Err(e),
        }
    }
}

/// One request, one reply.
pub fn call<Q: Serialize, R: DeserializeOwned>(name: &str, req: &Q) -> io::Result<R> {
    let mut f = connect(name, Duration::from_millis(500))?;
    write_msg(&mut f, req)?;
    read_msg(&mut f)
}

// ---------------------------------------------------------------------------------------------------------
// Indexer service protocol

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Rescan,
    Search { query: String, regex: bool, fuzzy: bool, limit: usize },
    /// Files selected by name filters (for content search).
    Files { filter: String, limit: usize },
    Largest { path: String, top: usize },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RowDto {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RootDto {
    pub root: String,
    pub mft: bool,
    pub entries: usize,
    pub phase: String,
    pub progress: f32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Pong { version: String, elevated: bool },
    Status { roots: Vec<RootDto> },
    Done,
    Search { total: usize, elapsed_us: u64, rows: Vec<RowDto> },
    Files { paths: Vec<String>, truncated: bool },
    Largest { files: Vec<(String, u64)>, folders: Vec<(String, u64)>, total_files: u64, total_bytes: u64 },
    Error { message: String },
}

fn row_dto(index: &Index, h: &crate::query::Hit) -> RowDto {
    let r = index.row(h);
    RowDto { path: r.path, name: r.name, is_dir: r.is_dir, size: r.size, mtime: r.mtime }
}

pub fn status_dto(ix: &Indexer) -> Vec<RootDto> {
    ix.status()
        .into_iter()
        .map(|s| RootDto {
            root: s.root,
            mft: s.kind == crate::index::VolumeKind::Mft,
            entries: s.entries,
            phase: match s.phase {
                Phase::Loading => "loading".into(),
                Phase::Scanning => "scanning".into(),
                Phase::Watching => "ready".into(),
                Phase::Failed(m) => format!("failed: {m}"),
            },
            progress: s.progress,
        })
        .collect()
}

/// Answers one request from the in-memory index.
pub fn handle(ix: &Indexer, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong { version: env!("CARGO_PKG_VERSION").into(), elevated: crate::winapi::is_elevated() },
        Request::Status => Response::Status { roots: status_dto(ix) },
        Request::Rescan => {
            ix.rescan();
            Response::Done
        }
        Request::Search { query, regex, fuzzy, limit } => {
            let opts = Options { regex, fuzzy, limit: limit.clamp(1, 5000), ..Default::default() };
            match Query::parse(&query, &opts) {
                Err(e) => Response::Error { message: e.to_string() },
                Ok(q) => {
                    let shared = ix.index();
                    let g = shared.read();
                    let res = g.search(&q, &opts);
                    Response::Search {
                        total: res.total,
                        elapsed_us: res.elapsed.as_micros() as u64,
                        rows: res.hits.iter().map(|h| row_dto(&g, h)).collect(),
                    }
                }
            }
        }
        Request::Files { filter, limit } => {
            let shared = ix.index();
            let g = shared.read();
            match crate::grep::candidates_from_index(&g, &filter) {
                Err(e) => Response::Error { message: e.to_string() },
                Ok(mut v) => {
                    let limit = limit.min(2_000_000);
                    let truncated = v.len() > limit;
                    v.truncate(limit);
                    Response::Files { paths: v.into_iter().map(|p| p.to_string_lossy().into_owned()).collect(), truncated }
                }
            }
        }
        Request::Largest { path, top } => {
            let shared = ix.index();
            let g = shared.read();
            match crate::largest::from_index(&g, &PathBuf::from(&path), top.clamp(1, 1000)) {
                None => Response::Error { message: "путь не покрыт индексом".into() },
                Some(l) => Response::Largest {
                    files: l.files.iter().map(|b| (b.path.to_string_lossy().into_owned(), b.size)).collect(),
                    folders: l.folders.iter().map(|b| (b.path.to_string_lossy().into_owned(), b.size)).collect(),
                    total_files: l.total_files,
                    total_bytes: l.total_bytes,
                },
            }
        }
    }
}

/// Serves the indexer protocol until `stop` is set. Each connection is handled on its own thread.
pub fn serve_indexer(ix: std::sync::Arc<Indexer>, name: &str, sddl: Option<&str>, stop: std::sync::Arc<AtomicBool>) {
    let mut server = PipeServer::new(name, sddl);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while !stop.load(Relaxed) {
        let Ok(mut conn) = server.accept() else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        if stop.load(Relaxed) {
            break;
        }
        if active.load(Relaxed) >= 32 {
            let _ = write_msg(&mut conn, &Response::Error { message: "сервер занят".into() });
            hang_up(conn);
            continue;
        }
        let (ix, active2) = (ix.clone(), active.clone());
        active.fetch_add(1, Relaxed);
        std::thread::spawn(move || {
            let mut conn = conn;
            // A client may keep the pipe open for several requests; a stalled client only blocks its own thread.
            while let Ok(req) = read_msg::<Request>(&mut conn) {
                if write_msg(&mut conn, &handle(&ix, req)).is_err() {
                    break;
                }
            }
            hang_up(conn);
            active2.fetch_sub(1, Relaxed);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::indexer::Config;

    #[test]
    fn framing_roundtrip_and_limits() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &Request::Search { query: "привет ext:txt".into(), regex: false, fuzzy: true, limit: 5 }).unwrap();
        let back: Request = read_msg(&mut buf.as_slice()).unwrap();
        assert!(matches!(back, Request::Search { ref query, limit: 5, .. } if query == "привет ext:txt"));
        // absurd length prefix is refused without allocating
        let evil = [0xFF, 0xFF, 0xFF, 0x7F];
        assert!(read_msg::<Request>(&mut evil.as_slice()).is_err());
        // truncated body
        assert!(read_msg::<Request>(&mut [5u8, 0, 0, 0, b'{'].as_slice()).is_err());
        // garbage JSON
        let mut g = Vec::new();
        g.extend_from_slice(&3u32.to_le_bytes());
        g.extend_from_slice(b"abc");
        assert!(read_msg::<Request>(&mut g.as_slice()).is_err());
    }

    #[test]
    fn service_protocol_over_a_real_pipe() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/Отчёт-2024.txt"), b"x").unwrap();
        std::fs::write(root.join("big.bin"), vec![0u8; 5000]).unwrap();
        let cfg = Config { data_file: tmp.path().join("ix.bin"), roots: vec![root.clone()], use_mft: false, save_interval: Duration::from_secs(3600) };
        let ix = Arc::new(Indexer::start(cfg));
        let t = Instant::now();
        while !ix.is_ready() && t.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let name = format!(r"\\.\pipe\StrelaTest-{}", std::process::id());
        let stop = Arc::new(AtomicBool::new(false));
        let (ix2, stop2, name2) = (ix.clone(), stop.clone(), name.clone());
        let server = std::thread::spawn(move || serve_indexer(ix2, &name2, Some(SERVICE_SDDL), stop2));
        std::thread::sleep(Duration::from_millis(100));

        let pong: Response = call(&name, &Request::Ping).unwrap();
        assert!(matches!(pong, Response::Pong { .. }));
        let r: Response = call(&name, &Request::Search { query: "отчёт ext:txt".into(), regex: false, fuzzy: false, limit: 10 }).unwrap();
        let Response::Search { total, rows, .. } = r else { panic!("{r:?}") };
        assert_eq!(total, 1);
        assert_eq!(rows[0].name, "Отчёт-2024.txt");
        let r: Response = call(&name, &Request::Search { query: "(".into(), regex: true, fuzzy: false, limit: 10 }).unwrap();
        assert!(matches!(r, Response::Error { .. }));
        let r: Response = call(&name, &Request::Files { filter: "ext:bin".into(), limit: 10 }).unwrap();
        assert!(matches!(r, Response::Files { ref paths, truncated: false } if paths.len() == 1));
        let r: Response = call(&name, &Request::Largest { path: root.to_string_lossy().into(), top: 5 }).unwrap();
        assert!(matches!(r, Response::Largest { total_files: 2, .. }), "{r:?}");
        let r: Response = call(&name, &Request::Status).unwrap();
        assert!(matches!(r, Response::Status { ref roots } if roots.len() == 1 && roots[0].phase == "ready"));

        // several requests on one connection
        let mut c = connect(&name, Duration::from_secs(1)).unwrap();
        for _ in 0..3 {
            write_msg(&mut c, &Request::Ping).unwrap();
            assert!(matches!(read_msg::<Response>(&mut c).unwrap(), Response::Pong { .. }));
        }
        drop(c);
        // a client that sends garbage does not kill the server
        {
            let mut c = connect(&name, Duration::from_secs(1)).unwrap();
            c.write_all(&[1, 0, 0, 0, b'!']).unwrap();
        }
        let again: Response = call(&name, &Request::Ping).unwrap();
        assert!(matches!(again, Response::Pong { .. }));

        stop.store(true, Relaxed);
        PipeServer::new(&name, None).poke();
        server.join().unwrap();
        ix.stop();
    }
}

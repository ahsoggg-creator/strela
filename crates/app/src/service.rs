//! `StrelaIndexer`: an optional Windows service that keeps the NTFS file index (MFT + USN journal) in memory and
//! answers search requests over a named pipe, so every Strela window answers instantly after a reboot and nobody has
//! to run elevated.
//!
//! Security: the service runs as LocalSystem, so its executable is copied to `%ProgramFiles%\Strela`, which only
//! administrators can write — never run from the user-writable install folder. The pipe only answers read-only
//! search requests and parses every message with a size limit. The index lists file *names* of every drive, like
//! Everything's service does; it does not open or read file contents.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::Duration;

use strela_core::indexer::{Config, Indexer};
use strela_core::ipc::{self, PipeServer, SERVICE_PIPE, SERVICE_SDDL};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const NAME: &str = "StrelaIndexer";
const DISPLAY: &str = "Strela — служба индексации файлов";
const DESCRIPTION: &str = "Держит в памяти индекс файлов NTFS (MFT и журнал USN) для мгновенного поиска Strela.";

define_windows_service!(ffi_service_main, service_main);

pub fn program_files_dir() -> PathBuf {
    std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files")).join("Strela")
}

pub fn service_exe() -> PathBuf {
    program_files_dir().join("StrelaIndexer.exe")
}

pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

pub fn is_running() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(NAME, ServiceAccess::QUERY_STATUS))
        .and_then(|s| s.query_status())
        .is_ok_and(|st| st.current_state == ServiceState::Running)
}

fn log_file() -> PathBuf {
    std::env::temp_dir().join("Strela-service.log")
}

fn log(msg: &str) {
    let _ = std::fs::write(log_file(), msg);
}

fn last_log() -> String {
    std::fs::read_to_string(log_file()).unwrap_or_default().trim().to_string()
}

/// One UAC prompt, then the elevated copy of this exe installs and starts the service.
pub fn install_via_uac(exe: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(log_file());
    match crate::winint::run_elevated(exe, "--install-service")? {
        0 => Ok(()),
        c => Err(format!("служба не установлена (код {c}) {}", last_log())),
    }
}

pub fn uninstall_via_uac() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // run a copy from %TEMP%: the installed exe is about to be deleted
    let tmp = std::env::temp_dir().join("Strela-remove-service.exe");
    std::fs::copy(&exe, &tmp).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(log_file());
    let r = crate::winint::run_elevated(&tmp, "--uninstall-service");
    let _ = std::fs::remove_file(&tmp);
    match r? {
        0 => Ok(()),
        c => Err(format!("служба не удалена (код {c}) {}", last_log())),
    }
}

fn wait_state(svc: &windows_service::service::Service, want: ServiceState, secs: u64) -> bool {
    for _ in 0..secs * 10 {
        if svc.query_status().is_ok_and(|s| s.current_state == want) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Runs elevated: copies this exe to Program Files, (re)creates the service and starts it.
pub fn install_elevated() -> Result<(), String> {
    let run = || -> Result<(), String> {
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        let dest = service_exe();
        std::fs::create_dir_all(program_files_dir()).map_err(|e| format!("{}: {e}", program_files_dir().display()))?;
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
            .map_err(|e| e.to_string())?;
        // upgrade: stop the old service before its exe is replaced
        if let Ok(old) = manager.open_service(NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE) {
            let _ = old.stop();
            wait_state(&old, ServiceState::Stopped, 15);
            let _ = old.delete();
            drop(old);
            std::thread::sleep(Duration::from_millis(500));
        }
        std::fs::copy(&me, &dest).map_err(|e| format!("копирование в {}: {e}", dest.display()))?;
        let info = ServiceInfo {
            name: OsString::from(NAME),
            display_name: OsString::from(DISPLAY),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: dest.clone(),
            launch_arguments: vec![OsString::from("--service")],
            dependencies: vec![],
            account_name: None, // LocalSystem
            account_password: None,
        };
        let svc = manager
            .create_service(&info, ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG)
            .map_err(|e| format!("создание службы: {e}"))?;
        let _ = svc.set_description(DESCRIPTION);
        svc.start::<&str>(&[]).map_err(|e| format!("запуск службы: {e}"))?;
        Ok(())
    };
    let r = run();
    if let Err(e) = &r {
        log(e);
    }
    r
}

pub fn uninstall_elevated() -> Result<(), String> {
    let run = || -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| e.to_string())?;
        if let Ok(svc) = manager.open_service(NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE) {
            let _ = svc.stop();
            wait_state(&svc, ServiceState::Stopped, 15);
            svc.delete().map_err(|e| format!("удаление службы: {e}"))?;
        }
        std::thread::sleep(Duration::from_millis(500));
        let dir = program_files_dir();
        // The service process may still be finishing (it writes its index while stopping): try for a few seconds.
        for attempt in 0..20 {
            if !dir.exists() {
                return Ok(());
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) if !dir.exists() => return Ok(()),
                Ok(()) => {}
                Err(e) if attempt == 19 => return Err(format!("{}: {e}", dir.display())),
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        if dir.exists() { Err(format!("{} не удалена", dir.display())) } else { Ok(()) }
    };
    let r = run();
    if let Err(e) = &r {
        log(e);
    }
    r
}

/// Entry point of `Strela.exe --service` (started by the service control manager).
pub fn run_dispatcher() -> Result<(), windows_service::Error> {
    service_dispatcher::start(NAME, ffi_service_main)
}

fn service_main(_args: Vec<OsString>) {
    let _ = run_service();
}

fn run_service() -> windows_service::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_handler = stop.clone();
    let handler = move |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop_for_handler.store(true, SeqCst);
            PipeServer::new(SERVICE_PIPE, None).poke();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(NAME, handler)?;
    let mut st = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status.set_service_status(st.clone())?;
    let data = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data").join("index.bin")))
        .unwrap_or_else(|| program_files_dir().join("data").join("index.bin"));
    let cfg = Config::all_fixed_drives(data, true);
    serve(cfg, SERVICE_PIPE, Some(SERVICE_SDDL), stop);
    st.current_state = ServiceState::Stopped;
    st.controls_accepted = ServiceControlAccept::empty();
    status.set_service_status(st)?;
    Ok(())
}

/// Starts the indexer and serves the pipe until `stop` is set.
pub fn serve(cfg: Config, pipe: &str, sddl: Option<&str>, stop: Arc<AtomicBool>) {
    if let Some(dir) = cfg.data_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let ix = Arc::new(Indexer::start(cfg));
    ipc::serve_indexer(ix.clone(), pipe, sddl, stop);
    ix.stop();
}

/// `Strela.exe --service-console [--pipe NAME] [--data FILE] [--root PATH]... [--no-mft]`: the same service loop in
/// the foreground (development, CI and diagnostics). Stops when the stop file `<data>.stop` appears.
pub fn run_console(args: &[String]) {
    let mut pipe = SERVICE_PIPE.to_string();
    let mut data: Option<PathBuf> = None;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut mft = true;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pipe" => pipe = it.next().cloned().unwrap_or(pipe),
            "--data" => data = it.next().map(PathBuf::from),
            "--root" => roots.extend(it.next().map(PathBuf::from)),
            "--no-mft" => mft = false,
            _ => {}
        }
    }
    let data = data.unwrap_or_else(|| strela_core::util::data_dir().join("service-index.bin"));
    let mut cfg = Config::all_fixed_drives(data.clone(), mft);
    if !roots.is_empty() {
        cfg.roots = roots;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_file = data.with_extension("stop");
    let _ = std::fs::remove_file(&stop_file);
    {
        let (stop, pipe) = (stop.clone(), pipe.clone());
        std::thread::spawn(move || {
            while !stop_file.exists() {
                std::thread::sleep(Duration::from_millis(200));
            }
            stop.store(true, SeqCst);
            PipeServer::new(&pipe, None).poke();
        });
    }
    eprintln!("strela service console: pipe {pipe}, mft={mft}");
    serve(cfg, &pipe, Some(SERVICE_SDDL), stop);
}

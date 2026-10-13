//! Commands from the Explorer context menu / Send To / the command line.
//!
//! Only one Strela process runs. Every `Strela.exe --cmd ...` launch forwards its command to that instance over a
//! named pipe and exits. Explorer starts one process per selected item, so the primary instance collects commands
//! of the same kind that arrive within a short window into one job.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use strela_core::ipc::{self, PipeServer};
use strela_core::transfer::Op;

use crate::state::{Shared, Tab, UiEvent};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Command {
    pub verb: String,
    pub paths: Vec<String>,
    /// Destination folder for `send-copy` / `send-move`.
    #[serde(default)]
    pub dest: Option<String>,
}

/// Quiet period after which a burst of per-item launches counts as complete.
const COLLECT_WINDOW: Duration = Duration::from_millis(450);

/// Explorer quotes `"C:\"` so that Windows argument parsing swallows the closing quote: `C:"` — repair it.
pub fn clean_path_arg(a: &str) -> String {
    let t = a.trim().trim_matches('"').trim();
    if t.len() == 2 && t.ends_with(':') { format!("{t}\\") } else { t.to_string() }
}

/// Parses `--cmd <verb> [--dest <dir>] [--] <paths...>`; returns `None` if the arguments hold no command.
pub fn parse_args(args: &[String]) -> Option<Command> {
    let i = args.iter().position(|a| a == "--cmd")?;
    let verb = args.get(i + 1)?.clone();
    let mut dest = None;
    let mut paths = Vec::new();
    let mut rest = args[i + 2..].iter();
    let mut only_paths = false;
    while let Some(a) = rest.next() {
        if !only_paths && a == "--" {
            only_paths = true;
        } else if !only_paths && a == "--dest" {
            dest = rest.next().map(|d| clean_path_arg(d));
        } else {
            let p = clean_path_arg(a);
            if !p.is_empty() {
                paths.push(p);
            }
        }
    }
    Some(Command { verb, paths, dest })
}

/// Sends the command to the running instance. `Err` means nobody is listening.
pub fn send_to_primary(cmd: &Command, wait: Duration) -> std::io::Result<()> {
    let start = Instant::now();
    loop {
        match ipc::connect(&ipc::user_pipe_name(), Duration::from_millis(300)) {
            Ok(mut f) => {
                ipc::write_msg(&mut f, cmd)?;
                let _ack: bool = ipc::read_msg(&mut f)?;
                return Ok(());
            }
            Err(e) if start.elapsed() < wait => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

static SENDER: OnceLock<Sender<Command>> = OnceLock::new();

/// Starts the pipe listener and the collector. Call once, in the primary instance.
pub fn start_server(shared: Arc<Shared>) {
    let (tx, rx) = channel::<Command>();
    let _ = SENDER.set(tx.clone());
    {
        let shared = shared.clone();
        std::thread::Builder::new().name("strela-collect".into()).spawn(move || collector(shared, rx)).expect("collector thread");
    }
    std::thread::Builder::new()
        .name("strela-ipc".into())
        .spawn(move || {
            let mut server = PipeServer::new(&ipc::user_pipe_name(), None);
            loop {
                let Ok(mut conn) = server.accept() else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                if let Ok(cmd) = ipc::read_msg::<Command>(&mut conn) {
                    let _ = ipc::write_msg(&mut conn, &true);
                    let _ = tx.send(cmd);
                }
                ipc::hang_up(conn);
            }
        })
        .expect("ipc thread");
}

/// Injects a command as if it had arrived over the pipe (used by the primary instance for its own launch).
pub fn submit_local(cmd: Command) {
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(cmd);
    }
}

fn collector(shared: Arc<Shared>, rx: Receiver<Command>) {
    let mut pending: Option<(Command, Instant)> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(cmd) => {
                let immediate = matches!(cmd.verb.as_str(), "spotlight" | "open" | "paste" | "find-here" | "find-content-here" | "largest-here");
                if immediate {
                    if let Some((p, _)) = pending.take() {
                        execute(&shared, p);
                    }
                    execute(&shared, cmd);
                    continue;
                }
                match &mut pending {
                    Some((p, t)) if p.verb == cmd.verb && p.dest == cmd.dest => {
                        for path in cmd.paths {
                            if !p.paths.contains(&path) {
                                p.paths.push(path);
                            }
                        }
                        *t = Instant::now();
                    }
                    _ => {
                        if let Some((p, _)) = pending.take() {
                            execute(&shared, p);
                        }
                        pending = Some((cmd, Instant::now()));
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if pending.as_ref().is_some_and(|(_, t)| t.elapsed() >= COLLECT_WINDOW)
                    && let Some((p, _)) = pending.take()
                {
                    execute(&shared, p);
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Runs one collected command.
pub fn execute(shared: &Arc<Shared>, cmd: Command) {
    let paths: Vec<PathBuf> = cmd.paths.iter().map(PathBuf::from).collect();
    match cmd.verb.as_str() {
        "copy" | "cut" => {
            let n = paths.len();
            let cut = cmd.verb == "cut";
            {
                let mut st = shared.staging.lock().unwrap();
                st.paths = paths;
                st.cut = cut;
            }
            shared.notify(
                "Strela",
                &format!(
                    "{} в буфере Strela: {n}. Откройте папку назначения → правый клик → Strela → Вставить.",
                    if cut { "Вырезано" } else { "Скопировано" }
                ),
                false,
            );
            shared.repaint();
        }
        "paste" => {
            if let Some(dest) = paths.into_iter().next() {
                shared.paste_into(dest);
                shared.push_event(UiEvent::Show(Tab::Transfer));
            }
        }
        "move-to" | "copy-to" => {
            let op = if cmd.verb == "move-to" { Op::Move } else { Op::Copy };
            shared.push_event(UiEvent::PickDestination { paths, op });
            shared.show_window();
        }
        "fast-delete" | "recycle" => {
            shared.push_event(UiEvent::ConfirmDelete { paths, recycle: cmd.verb == "recycle" });
            shared.show_window();
        }
        "find-here" => {
            if let Some(p) = paths.into_iter().next() {
                shared.push_event(UiEvent::FindHere(p));
                shared.show_window();
            }
        }
        "find-content-here" => {
            if let Some(p) = paths.into_iter().next() {
                shared.push_event(UiEvent::FindContentHere(p));
                shared.show_window();
            }
        }
        "largest-here" => {
            if let Some(p) = paths.into_iter().next() {
                shared.push_event(UiEvent::LargestHere(p));
                shared.show_window();
            }
        }
        "send-copy" | "send-move" => {
            if let Some(dest) = cmd.dest {
                let op = if cmd.verb == "send-move" { Op::Move } else { Op::Copy };
                shared.start_transfer(paths, PathBuf::from(dest), op);
            }
        }
        "quit" => {
            // reply has been sent already; leave the pipe thread before tearing everything down
            std::thread::spawn({
                let sh = shared.clone();
                move || {
                    std::thread::sleep(Duration::from_millis(100));
                    sh.quit();
                }
            });
        }
        "spotlight" => {
            shared.push_event(UiEvent::Spotlight);
            crate::shell::show_main(shared);
        }
        _ => {
            shared.push_event(UiEvent::Show(Tab::Search));
            shared.show_window();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_commands() {
        let c = parse_args(&args(&["Strela.exe", "--cmd", "copy", "--", r"C:\a b\c.txt"])).unwrap();
        assert_eq!(c, Command { verb: "copy".into(), paths: vec![r"C:\a b\c.txt".into()], dest: None });
        let c = parse_args(&args(&["x", "--cmd", "send-move", "--dest", r"D:\", "--", r"C:\x", r"C:\y"])).unwrap();
        assert_eq!(c.dest.as_deref(), Some(r"D:\"));
        assert_eq!(c.paths.len(), 2);
        assert!(parse_args(&args(&["x", "--hidden"])).is_none());
        // drive root as Explorer delivers it: C:\" -> C:"
        let c = parse_args(&args(&["x", "--cmd", "largest-here", "--", "C:\""])).unwrap();
        assert_eq!(c.paths, vec![r"C:\"]);
        // a path that looks like a flag is still a path after `--`
        let c = parse_args(&args(&["x", "--cmd", "copy", "--", "--dest"])).unwrap();
        assert_eq!(c.paths, vec!["--dest"]);
    }

    #[test]
    fn command_survives_json() {
        let c = Command { verb: "recycle".into(), paths: vec!["C:\\Файлы\\а.txt".into()], dest: None };
        let mut buf = Vec::new();
        ipc::write_msg(&mut buf, &c).unwrap();
        assert_eq!(ipc::read_msg::<Command>(&mut buf.as_slice()).unwrap(), c);
    }
}

//! System tools from the console: installed programs (list / uninstall / traces), startup entries, processes.

use std::io::Write;
use std::time::Duration;

use clap::Subcommand;
use strela_core::apps::{self, Mode};
use strela_core::leftovers::{self, Confidence, Subject};
use strela_core::procs::{self, Sampler};
use strela_core::progress::Progress;
use strela_core::startup::{self, Group, Kind};
use strela_core::util;
use strela_core::winapi;

type Res = Result<(), Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub enum SysCmd {
    /// Установленные программы (с фильтром по названию или издателю)
    Apps {
        filter: Option<String>,
    },
    /// Удалить программу и (по умолчанию) найти её следы: `strela-cli uninstall "Notepad++" --force --clean`
    Uninstall {
        /// Название (часть) — должно подойти ровно одной программе
        name: String,
        /// Принудительно: без деинсталлятора — закрыть процессы, удалить папку и запись
        #[arg(long)]
        force: bool,
        /// Деинсталлятор с окнами вместо тихого режима
        #[arg(long)]
        windows: bool,
        /// Сразу удалить надёжно найденные следы (без этого — только показать)
        #[arg(long)]
        clean: bool,
        /// Не искать следы
        #[arg(long)]
        no_traces: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Найти следы программы по названию (в том числе уже удалённой)
    Traces {
        name: String,
        /// Удалить надёжные следы (реестр сохраняется в .reg)
        #[arg(long)]
        clean: bool,
        /// Удалить и вероятные следы тоже
        #[arg(long)]
        all: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Автозагрузка: реестр, папки, службы, задачи
    Startup {
        /// Показать и системные записи Microsoft и ручные службы
        #[arg(long)]
        all: bool,
        /// Только записи с предупреждением (битые, из временных папок)
        #[arg(long)]
        issues: bool,
    },
    /// Процессы с ЦП, памятью и командной строкой
    Ps {
        filter: Option<String>,
        #[arg(long, short = 'n', default_value_t = 25)]
        top: usize,
        /// Показать командные строки
        #[arg(long, short = 'c')]
        cmdline: bool,
    },
    /// Завершить процесс по PID
    Kill {
        pid: u32,
        /// Вместе с дочерними процессами
        #[arg(long)]
        tree: bool,
    },
}

pub fn run(cmd: SysCmd) -> Res {
    match cmd {
        SysCmd::Apps { filter } => list_apps(filter.as_deref()),
        SysCmd::Uninstall { name, force, windows, clean, no_traces, yes } => uninstall(&name, force, windows, clean, no_traces, yes),
        SysCmd::Traces { name, clean, all, yes } => traces(&name, clean, all, yes),
        SysCmd::Startup { all, issues } => show_startup(all, issues),
        SysCmd::Ps { filter, top, cmdline } => ps(filter.as_deref(), top, cmdline),
        SysCmd::Kill { pid, tree } => kill(pid, tree),
    }
}

fn matches(filter: Option<&str>, hay: &[&str]) -> bool {
    let Some(f) = filter else { return true };
    let joined = hay.join("\u{1}").to_lowercase();
    f.to_lowercase().split_whitespace().all(|w| joined.contains(w))
}

fn list_apps(filter: Option<&str>) -> Res {
    let all = apps::list();
    let shown: Vec<_> = all.iter().filter(|a| matches(filter, &[&a.name, &a.publisher])).collect();
    for a in &shown {
        println!(
            "{:<46} {:<14} {:<26} {:>9}  {}",
            trunc(&a.name, 46),
            trunc(&a.version, 14),
            trunc(&a.publisher, 26),
            if a.size_bytes > 0 { util::bytes(a.size_bytes) } else { String::new() },
            a.install_location.as_ref().map(|p| winapi::display_path(p)).unwrap_or_default()
        );
    }
    println!("— {} из {}", shown.len(), all.len());
    Ok(())
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

fn pick_app(name: &str) -> Result<apps::App, Box<dyn std::error::Error>> {
    let all = apps::list();
    let n = name.to_lowercase();
    let exact: Vec<_> = all.iter().filter(|a| a.name.to_lowercase() == n).collect();
    let found: Vec<_> = if exact.len() == 1 { exact } else { all.iter().filter(|a| a.name.to_lowercase().contains(&n)).collect() };
    match found.len() {
        0 => Err(format!("программа «{name}» не найдена (список: strela-cli apps)").into()),
        1 => Ok(found[0].clone()),
        k => Err(format!("под «{name}» подходит {k} программ: {}. Уточните название.", found.iter().take(6).map(|a| format!("«{}»", a.name)).collect::<Vec<_>>().join(", ")).into()),
    }
}

fn confirm(question: &str, yes: bool) -> bool {
    if yes {
        return true;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).is_ok() && matches!(s.trim().to_lowercase().as_str(), "y" | "yes" | "д" | "да")
}

fn uninstall(name: &str, force: bool, windows: bool, clean: bool, no_traces: bool, yes: bool) -> Res {
    let app = pick_app(name)?;
    let mode = if force { Mode::Force } else if windows { Mode::Interactive } else { Mode::Quiet };
    println!("{} {}  [{}]", app.name, app.version, app.publisher);
    if let Some(d) = &app.install_location {
        println!("папка: {}", winapi::display_path(d));
    }
    if !confirm(&format!("Удалить «{}» ({})?", app.name, match mode { Mode::Force => "принудительно", Mode::Interactive => "с окнами", Mode::Quiet => "тихо" }), yes) {
        println!("отменено");
        return Ok(());
    }
    let progress = Progress::default();
    let rep = apps::uninstall(&app, mode, &progress, &mut |s| println!("  {s}"))?;
    if let Some(c) = rep.uninstaller_exit {
        println!("деинсталлятор: код {c}");
    }
    if rep.killed > 0 {
        println!("закрыто процессов: {}", rep.killed);
    }
    if let Some(d) = &rep.folder_removed {
        println!("папка {} удалена: {} за {:.2} с", winapi::display_path(d), util::bytes(rep.bytes), rep.elapsed.as_secs_f64());
    }
    for n in &rep.notes {
        println!("! {n}");
    }
    if rep.still_registered {
        println!("! запись о программе осталась в списке");
    }
    if no_traces {
        return Ok(());
    }
    traces_for(Subject::from_app(&app), clean, false, yes)
}

fn traces(name: &str, clean: bool, all: bool, yes: bool) -> Res {
    // a program that is still installed gets its full subject (folder, key); otherwise the name alone
    let subject = apps::list().into_iter().find(|a| a.name.to_lowercase() == name.to_lowercase()).map_or_else(|| Subject::by_name(name), |a| Subject::from_app(&a));
    traces_for(subject, clean, all, yes)
}

fn traces_for(subject: Subject, clean: bool, all: bool, yes: bool) -> Res {
    let progress = Progress::default();
    let found = leftovers::scan(&subject, &progress);
    if found.is_empty() {
        println!("следов не найдено");
        return Ok(());
    }
    for t in &found {
        println!(
            "{} {:<18} {}  {}",
            if t.confidence == Confidence::High { "●" } else { "○" },
            t.kind,
            t.label,
            if t.size > 0 { format!("({})", util::bytes(t.size)) } else { String::new() }
        );
    }
    let picked: Vec<_> = found.iter().filter(|t| all || t.confidence == Confidence::High).cloned().collect();
    println!("— найдено {} (● надёжные: {}, ○ вероятные: {})", found.len(), found.iter().filter(|t| t.confidence == Confidence::High).count(), found.iter().filter(|t| t.confidence == Confidence::Medium).count());
    if !clean {
        println!("чтобы удалить: добавьте --clean (надёжные) или --clean --all (все)");
        return Ok(());
    }
    if !confirm(&format!("Удалить {} объектов?", picked.len()), yes) {
        println!("отменено");
        return Ok(());
    }
    let rep = leftovers::clean(&picked, &subject.name, &progress);
    println!("удалено: {} ({}) за {:.2} с", rep.removed, util::bytes(rep.bytes), rep.elapsed.as_secs_f64());
    for (w, why) in &rep.failed {
        println!("! {w}: {why}");
    }
    if let Some(b) = &rep.backup {
        println!("копии реестра: {}", winapi::display_path(b));
    }
    Ok(())
}

fn show_startup(all: bool, issues: bool) -> Res {
    let list = startup::list();
    let mut shown = 0;
    for g in Group::ALL {
        let items: Vec<_> = list
            .iter()
            .filter(|e| e.group == g)
            .filter(|e| all || !(e.system || (g == Group::Service && e.is_microsoft())))
            .filter(|e| all || !matches!(&e.kind, Kind::Service { start: startup::StartType::Manual | startup::StartType::Disabled, .. }))
            .filter(|e| !issues || e.warning.is_some())
            .collect();
        if items.is_empty() {
            continue;
        }
        println!("\n== {} ({})", g.title(), items.len());
        for e in items {
            shown += 1;
            println!("{} {:<34} {:<26} {}", if e.enabled { "[вкл]" } else { "[выкл]" }, trunc(&e.name, 34), trunc(e.company.as_deref().unwrap_or(""), 26), trunc(&e.command, 70));
            if let Some(w) = &e.warning {
                println!("      ⚠ {w}");
            }
        }
    }
    println!("\n— записей: {shown} из {}", list.len());
    Ok(())
}

fn ps(filter: Option<&str>, top: usize, cmdline: bool) -> Res {
    let mut s = Sampler::new();
    s.snapshot()?;
    std::thread::sleep(Duration::from_millis(700));
    let mut all = s.snapshot()?;
    all.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal).then(b.working_set.cmp(&a.working_set)));
    println!("{:>7}  {:>6} {:>10} {:>10} {:>5}  {:<28} пользователь", "PID", "ЦП", "память", "частная", "потк", "имя");
    for (n, p) in all.iter().filter(|p| matches(filter, &[&p.name, &p.pid.to_string(), p.user.as_deref().unwrap_or(""), &p.path.as_ref().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default()])).enumerate() {
        if n >= top {
            break;
        }
        println!("{:>7}  {:>5.1}% {:>10} {:>10} {:>5}  {:<28} {}", p.pid, p.cpu, util::bytes(p.working_set), util::bytes(p.private_bytes), p.threads, trunc(&p.name, 28), p.user.as_deref().unwrap_or(""));
        if cmdline && let Some(c) = procs::command_line(p.pid) {
            println!("         {c}");
        }
    }
    println!("— процессов всего: {}", all.len());
    Ok(())
}

fn kill(pid: u32, tree: bool) -> Res {
    let mut s = Sampler::new();
    let all = s.snapshot()?;
    let me = s.own_pid();
    let target = all.iter().find(|p| p.pid == pid).ok_or_else(|| format!("процесса {pid} нет"))?;
    if let Some(why) = procs::protected_reason(target, me) {
        return Err(format!("{} ({pid}): нельзя завершить — {why}", target.name).into());
    }
    let pids = if tree { procs::tree_pids(pid, &all) } else { vec![pid] };
    let (done, failed) = procs::terminate_many(&pids, &all, me);
    println!("завершено процессов: {done}");
    for (p, e) in &failed {
        println!("! {p}: {e}");
    }
    if failed.is_empty() { Ok(()) } else { Err("часть процессов не завершена".into()) }
}

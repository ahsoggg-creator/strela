//! `strela-cli` — the Strela engine without a GUI: search, copy, move, delete, grep, biggest files.

mod bench;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use strela_core::delete;
use strela_core::grep::{self, GrepOptions};
use strela_core::guard::Guard;
use strela_core::index::VolumeKind;
use strela_core::indexer::{Config, Indexer, Phase};
use strela_core::largest;
use strela_core::progress::Progress;
use strela_core::query::{Options as QueryOptions, Query};
use strela_core::transfer::{self, Control, Engine, HashAlgo, JobSpec, Op, Options, Overwrite, TransferProgress};
use strela_core::util;
use strela_core::winapi;

#[derive(Parser)]
#[command(name = "strela-cli", version, about = "Strela: быстрые файловые операции из консоли")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Мгновенный поиск файлов по имени: `strela-cli search "отчёт ext:docx size:>1MB modified:<30d"`
    Search {
        /// Слова (все должны входить в имя) и фильтры ext:, size:, modified:, path:
        query: String,
        /// Что индексировать (по умолчанию — все локальные диски). Можно повторять.
        #[arg(long = "root", short = 'r')]
        roots: Vec<PathBuf>,
        /// Запрос — регулярное выражение для имени
        #[arg(long)]
        regex: bool,
        /// Без нечёткого совпадения
        #[arg(long)]
        no_fuzzy: bool,
        #[arg(long, short = 'n', default_value_t = 50)]
        limit: usize,
        /// Читать MFT напрямую (нужны права администратора)
        #[arg(long)]
        mft: bool,
        /// Дождаться свежего сканирования вместо сохранённого индекса
        #[arg(long)]
        wait: bool,
        /// Файл индекса (по умолчанию %LOCALAPPDATA%\Strela\cli-index.bin)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Печатать время
        #[arg(long)]
        time: bool,
        /// Спросить службу индексации StrelaIndexer вместо собственного индекса
        #[arg(long)]
        service: bool,
        /// Имя канала службы (для отладки; по умолчанию канал StrelaIndexer)
        #[arg(long)]
        pipe: Option<String>,
    },
    /// Построить индекс и сохранить его (с замером времени)
    Index {
        #[arg(long = "root", short = 'r')]
        roots: Vec<PathBuf>,
        #[arg(long)]
        mft: bool,
        #[arg(long)]
        index: Option<PathBuf>,
    },
    /// Копировать
    Copy(TransferArgs),
    /// Переместить (в пределах тома — мгновенно; между томами — копирование + проверка + удаление)
    Move(TransferArgs),
    /// Быстро удалить (по умолчанию безвозвратно; --recycle — в корзину)
    Delete {
        paths: Vec<PathBuf>,
        #[arg(long)]
        recycle: bool,
        /// Подтвердить без вопроса (итоговые числа всё равно печатаются)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Имя защищённой папки для подтверждения (см. защиту системных путей)
        #[arg(long)]
        type_name: Option<String>,
    },
    /// Поиск по содержимому файлов
    Grep {
        pattern: String,
        /// Папка поиска
        #[arg(long, short = 'r', default_value = ".")]
        root: PathBuf,
        /// Фильтры имён: "ext:rs size:<1MB modified:<30d"
        #[arg(long, short = 'f', default_value = "")]
        filter: String,
        #[arg(long)]
        regex: bool,
        #[arg(long, short = 's')]
        case_sensitive: bool,
        #[arg(long, default_value_t = 20)]
        max_per_file: usize,
    },
    /// Самые большие файлы и папки
    Largest {
        path: PathBuf,
        #[arg(long, short = 'n', default_value_t = 20)]
        top: usize,
    },
    #[command(hide = true)]
    Bench(bench::BenchCmd),
}

#[derive(clap::Args)]
struct TransferArgs {
    /// Источники, затем (последним аргументом) папка назначения
    #[arg(required = true, num_args = 2..)]
    paths: Vec<PathBuf>,
    #[arg(long, value_enum)]
    verify: Option<VerifyArg>,
    #[arg(long, value_enum, default_value = "auto")]
    engine: EngineArg,
    #[arg(long, value_enum, default_value = "replace")]
    overwrite: OverwriteArg,
    /// Жёсткие ссылки вместо копий (в пределах одного тома)
    #[arg(long)]
    hardlink: bool,
    /// Продолжить прерванную передачу
    #[arg(long)]
    resume: bool,
    /// Потоков для мелких файлов (0 — авто)
    #[arg(long, default_value_t = 0)]
    threads: usize,
    /// Подтверждение для перемещения защищённой папки
    #[arg(long)]
    type_name: Option<String>,
    /// Не печатать полосу прогресса
    #[arg(long)]
    quiet: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum VerifyArg {
    Blake3,
    Xxh3,
}
#[derive(Clone, Copy, ValueEnum)]
enum EngineArg {
    Auto,
    Pipeline,
    Copyfile2,
}
#[derive(Clone, Copy, ValueEnum)]
enum OverwriteArg {
    Replace,
    Skip,
    Newer,
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli.cmd) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ошибка: {e}");
            1
        }
    };
    std::process::exit(code);
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn run(cmd: Cmd) -> Res {
    match cmd {
        Cmd::Search { query, roots, regex, no_fuzzy, limit, mft, wait, index, time, service, pipe } => {
            if service || pipe.is_some() {
                return search_service(&query, regex, no_fuzzy, limit, time, pipe);
            }
            search(&query, roots, regex, no_fuzzy, limit, mft, wait, index, time)
        }
        Cmd::Index { roots, mft, index } => build_index(roots, mft, index),
        Cmd::Copy(a) => transfer_cmd(Op::Copy, a),
        Cmd::Move(a) => transfer_cmd(Op::Move, a),
        Cmd::Delete { paths, recycle, yes, type_name } => delete_cmd(paths, recycle, yes, type_name),
        Cmd::Grep { pattern, root, filter, regex, case_sensitive, max_per_file } => {
            grep_cmd(pattern, root, filter, regex, case_sensitive, max_per_file)
        }
        Cmd::Largest { path, top } => largest_cmd(path, top),
        Cmd::Bench(b) => bench::run(b),
    }
}

fn indexer_config(roots: Vec<PathBuf>, mft: bool, index: Option<PathBuf>) -> Config {
    let data_file = index.unwrap_or_else(|| util::data_dir().join("cli-index.bin"));
    let mut cfg = Config::all_fixed_drives(data_file, mft);
    if !roots.is_empty() {
        cfg.roots = roots;
    }
    cfg
}

fn wait_for_scan(ix: &Indexer, until_ready: bool) {
    let t0 = Instant::now();
    let mut last = String::new();
    loop {
        let st = ix.status();
        let busy = st.iter().any(|s| matches!(s.phase, Phase::Scanning | Phase::Loading));
        let have_any = st.iter().any(|s| s.entries > 0 && !matches!(s.phase, Phase::Scanning));
        if !busy || (!until_ready && have_any && t0.elapsed() > Duration::from_millis(50)) {
            break;
        }
        let line = st
            .iter()
            .map(|s| format!("{} {}{}", s.root, s.entries, if s.phase == Phase::Scanning { "…" } else { "" }))
            .collect::<Vec<_>>()
            .join("  ");
        if line != last && std::io::stderr().is_terminal() {
            eprint!("\rиндексация: {line}   ");
            last = line;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if std::io::stderr().is_terminal() {
        eprint!("\r{:80}\r", "");
    }
}

#[allow(clippy::too_many_arguments)]
fn search(q: &str, roots: Vec<PathBuf>, regex: bool, no_fuzzy: bool, limit: usize, mft: bool, wait: bool, index: Option<PathBuf>, time: bool) -> Res {
    let ix = Indexer::start(indexer_config(roots, mft, index));
    wait_for_scan(&ix, wait);
    let opts = QueryOptions { regex, fuzzy: !no_fuzzy, limit, ..Default::default() };
    let query = Query::parse(q, &opts)?;
    let shared = ix.index();
    let g = shared.read();
    let res = g.search(&query, &opts);
    let mut out = std::io::stdout().lock();
    for h in &res.hits {
        let r = g.row(h);
        let size = if r.is_dir { "<папка>".to_string() } else { util::bytes(r.size) };
        writeln!(out, "{:>10}  {}  {}", size, util::date(r.mtime), r.path)?;
    }
    if time {
        let total: usize = g.len();
        let kinds: Vec<String> =
            g.volumes.iter().map(|v| format!("{}:{}", v.root, if v.kind == VolumeKind::Mft { "MFT" } else { "обход" })).collect();
        eprintln!(
            "{} совпадений ({} показано) за {:.2} мс; в индексе {} записей [{}]",
            res.total,
            res.hits.len(),
            res.elapsed.as_secs_f64() * 1000.0,
            total,
            kinds.join(" ")
        );
    }
    drop(g);
    ix.stop();
    Ok(())
}

fn search_service(q: &str, regex: bool, no_fuzzy: bool, limit: usize, time: bool, pipe: Option<String>) -> Res {
    use strela_core::ipc::{self, Request, Response};
    let name = pipe.unwrap_or_else(|| ipc::SERVICE_PIPE.to_string());
    let t = Instant::now();
    let resp: Response = ipc::call(&name, &Request::Search { query: q.into(), regex, fuzzy: !no_fuzzy, limit })
        .map_err(|e| format!("служба индексации недоступна ({name}): {e}"))?;
    match resp {
        Response::Search { total, elapsed_us, rows } => {
            let mut out = std::io::stdout().lock();
            for r in &rows {
                let size = if r.is_dir { "<папка>".to_string() } else { util::bytes(r.size) };
                writeln!(out, "{:>10}  {}  {}", size, util::date(r.mtime), r.path)?;
            }
            if time {
                eprintln!(
                    "{total} совпадений ({} показано): поиск в службе {:.2} мс, с каналом {:.2} мс",
                    rows.len(),
                    elapsed_us as f64 / 1000.0,
                    t.elapsed().as_secs_f64() * 1000.0
                );
            }
            Ok(())
        }
        Response::Error { message } => Err(message.into()),
        other => Err(format!("неожиданный ответ: {other:?}").into()),
    }
}

fn build_index(roots: Vec<PathBuf>, mft: bool, index: Option<PathBuf>) -> Res {
    let t0 = Instant::now();
    // a fresh scan, not the snapshot
    let mut cfg = indexer_config(roots, mft, index);
    let _ = std::fs::remove_file(&cfg.data_file);
    cfg.save_interval = Duration::from_secs(3600);
    let file = cfg.data_file.clone();
    let ix = Indexer::start(cfg);
    wait_for_scan(&ix, true);
    for s in ix.status() {
        println!(
            "{}  {:>10} записей  {}  {}",
            s.root,
            s.entries,
            if s.kind == VolumeKind::Mft { "MFT+USN" } else { "обход папок" },
            s.last_scan.map_or(String::new(), |d| format!("{:.2} с", d.as_secs_f64()))
        );
        if let Phase::Failed(m) = s.phase {
            println!("  ошибка: {m}");
        }
    }
    ix.stop();
    println!("всего {:.2} с; индекс: {} ({})", t0.elapsed().as_secs_f64(), file.display(), util::bytes(std::fs::metadata(&file).map_or(0, |m| m.len())));
    Ok(())
}

fn transfer_cmd(op: Op, a: TransferArgs) -> Res {
    let mut paths = a.paths;
    let dest = paths.pop().expect("clap guarantees two or more paths");
    let spec = JobSpec {
        op,
        sources: paths,
        dest_dir: dest,
        options: Options {
            overwrite: match a.overwrite {
                OverwriteArg::Replace => Overwrite::Replace,
                OverwriteArg::Skip => Overwrite::Skip,
                OverwriteArg::Newer => Overwrite::Newer,
            },
            verify: a.verify.map(|v| match v {
                VerifyArg::Blake3 => HashAlgo::Blake3,
                VerifyArg::Xxh3 => HashAlgo::Xxh3,
            }),
            hardlink: a.hardlink,
            clone: true,
            engine: match a.engine {
                EngineArg::Auto => Engine::Auto,
                EngineArg::Pipeline => Engine::Pipeline,
                EngineArg::Copyfile2 => Engine::CopyFile2,
            },
            resume: a.resume,
            small_threads: a.threads,
        },
        typed_confirm: a.type_name,
    };
    let ctrl = Control::default();
    let prog = TransferProgress::default();
    let guard = Guard::system();
    let t0 = Instant::now();
    let rep = std::thread::scope(|s| {
        let h = s.spawn(|| transfer::run(&spec, &guard, &ctrl, &prog));
        let tty = std::io::stderr().is_terminal() && !a.quiet;
        while !h.is_finished() {
            if tty {
                let p = prog.snapshot();
                let pct = if p.bytes_total > 0 { p.bytes_done as f64 * 100.0 / p.bytes_total as f64 } else { 0.0 };
                let filled = (pct / 100.0 * 24.0) as usize;
                eprint!(
                    "\r[{}{}] {:5.1}%  {}  {}/{} файлов  ETA {}   ",
                    "#".repeat(filled),
                    "-".repeat(24 - filled),
                    pct,
                    util::speed(p.speed),
                    p.files_done,
                    p.files_total,
                    p.eta.map_or("--".into(), util::clock)
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if tty {
            eprint!("\r{:100}\r", "");
        }
        h.join().expect("transfer thread")
    });
    let rep = rep?;
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "готово за {:.2} с: {} файлов, {} ({}); пропущено {}; ошибок {}",
        secs,
        rep.files_copied,
        util::bytes(rep.bytes),
        util::speed(if secs > 0.0 { rep.bytes as f64 / secs } else { 0.0 }),
        rep.files_skipped,
        rep.errors.len()
    );
    for n in &rep.notes {
        println!("  • {n}");
    }
    if rep.cloned > 0 {
        println!("  • клонирование блоков: {} файлов", rep.cloned);
    }
    if rep.linked > 0 {
        println!("  • жёстких ссылок: {}", rep.linked);
    }
    for e in rep.errors.iter().take(20) {
        eprintln!("  ✗ {}: {}", e.path.display(), e.message);
    }
    if !rep.errors.is_empty() {
        return Err(format!("{} элементов не обработано", rep.errors.len()).into());
    }
    Ok(())
}

fn ask(prompt: &str) -> String {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

fn delete_cmd(paths: Vec<PathBuf>, recycle: bool, yes: bool, type_name: Option<String>) -> Res {
    if paths.is_empty() {
        return Err("не указаны пути".into());
    }
    let guard = Guard::system();
    let prog = Progress::default();
    let pv = delete::preview(&paths, &guard, &prog)?;
    println!(
        "К удалению ({}): {} файлов, {} папок, {}{}",
        if recycle { "в корзину" } else { "БЕЗВОЗВРАТНО" },
        pv.files,
        pv.dirs,
        util::bytes(pv.bytes),
        if pv.links > 0 { format!(", ссылок {} (удаляются сами ссылки)", pv.links) } else { String::new() }
    );
    let mut typed = type_name;
    if let Some(p) = pv.needs_typed_confirmation() {
        println!("ЗАЩИЩЁННОЕ МЕСТО: {}", p.reason);
        if typed.is_none() && std::io::stdin().is_terminal() {
            typed = Some(ask(&format!("Для подтверждения введите «{}»: ", p.required_name)));
        }
    } else if !yes {
        let a = ask("Удалить? [y/N] ");
        if !matches!(a.to_lowercase().as_str(), "y" | "yes" | "д" | "да") {
            return Err("отменено".into());
        }
    }
    let confirmed = pv.confirm(typed.as_deref())?;
    let rep = if recycle { delete::delete_to_recycle_bin(&confirmed)? } else { delete::delete_permanently(&confirmed, &Progress::default())? };
    println!(
        "удалено за {:.2} с: {} файлов, {} папок, {}",
        rep.elapsed.as_secs_f64(),
        rep.files,
        rep.dirs,
        util::bytes(rep.bytes)
    );
    for (p, m) in rep.failed.iter().take(20) {
        eprintln!("  ✗ {}: {m}", p.display());
    }
    if rep.failed.is_empty() { Ok(()) } else { Err(format!("{} элементов не удалено", rep.failed.len()).into()) }
}

fn grep_cmd(pattern: String, root: PathBuf, filter: String, regex: bool, case_sensitive: bool, max_per_file: usize) -> Res {
    let prog = Progress::default();
    let t0 = Instant::now();
    let root = std::fs::canonicalize(&root).map(|p| PathBuf::from(winapi::display_path(&p))).unwrap_or(root);
    let files = grep::candidates_from_walk(&root, &filter, &prog)?;
    let t_walk = t0.elapsed();
    let out = std::sync::Mutex::new(std::io::stdout());
    let stats = grep::grep(
        &files,
        &GrepOptions { pattern, regex, case_insensitive: !case_sensitive, max_matches_per_file: max_per_file, max_filesize: None },
        &Progress::default(),
        &|hits| {
            let mut o = out.lock().unwrap();
            for m in &hits.matches {
                let _ = writeln!(o, "{}:{}:{}", hits.path.display(), m.line, m.text.trim());
            }
        },
    )?;
    eprintln!(
        "{} совпадений в {} файлах; просмотрено {} файлов (бинарных пропущено {}); обход {:.2} с, всего {:.2} с",
        stats.matches,
        stats.files_with_matches,
        stats.files_searched,
        stats.binary_skipped,
        t_walk.as_secs_f64(),
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

fn largest_cmd(path: PathBuf, top: usize) -> Res {
    let t0 = Instant::now();
    let l = largest::from_walk(&path, top, &Progress::default())?;
    println!("Самые большие файлы:");
    for b in &l.files {
        println!("{:>10}  {}", util::bytes(b.size), b.path.display());
    }
    println!("\nСамые большие папки:");
    for b in &l.folders {
        println!("{:>10}  {}", util::bytes(b.size), b.path.display());
    }
    println!("\nвсего {} файлов, {}; {:.2} с", l.total_files, util::bytes(l.total_bytes), t0.elapsed().as_secs_f64());
    let _ = Relaxed;
    Ok(())
}

//! Content search, ripgrep-style: the `grep-searcher` / `grep-regex` engine on all cores. Binary files are skipped
//! (a NUL byte quits the file), UTF-16 files with a BOM are transcoded, and candidates are chosen with the same
//! filters as the name search (`ext:`, `size:`, `modified:`, `path:`, name words) — from the in-memory index when
//! available, otherwise by walking the folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::index::Index;
use crate::progress::Progress;
use crate::query::{Options as QueryOptions, Query};
use crate::winapi::{self, RawEntry};

#[derive(Clone, Debug)]
pub struct GrepOptions {
    pub pattern: String,
    /// Treat `pattern` as a regular expression (otherwise a plain string).
    pub regex: bool,
    pub case_insensitive: bool,
    pub max_matches_per_file: usize,
    /// Files larger than this are skipped.
    pub max_filesize: Option<u64>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        GrepOptions { pattern: String::new(), regex: false, case_insensitive: true, max_matches_per_file: 200, max_filesize: None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// 1-based line number.
    pub line: u64,
    /// The line (trimmed of the line ending, shortened around the first hit when very long).
    pub text: String,
    /// Byte ranges of the hits inside `text`.
    pub ranges: Vec<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub struct FileHits {
    pub path: PathBuf,
    pub matches: Vec<Match>,
}

#[derive(Clone, Debug, Default)]
pub struct GrepStats {
    pub files_searched: u64,
    pub files_with_matches: u64,
    pub matches: u64,
    pub binary_skipped: u64,
    pub too_large: u64,
    pub errors: u64,
}

const MAX_LINE_SHOWN: usize = 400;

struct Collect<'a> {
    matcher: &'a RegexMatcher,
    out: Vec<Match>,
    max: usize,
    binary: bool,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, m: &SinkMatch<'_>) -> std::result::Result<bool, Self::Error> {
        let bytes = m.bytes();
        let trimmed = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        let mut hits: Vec<(usize, usize)> = Vec::new();
        let _ = self.matcher.find_iter(trimmed, |h| {
            hits.push((h.start(), h.end()));
            hits.len() < 64
        });
        // Long lines (minified files): show a window around the first hit, never cut inside a UTF-8 sequence.
        let (mut start, mut end) = (0usize, trimmed.len());
        if trimmed.len() > MAX_LINE_SHOWN {
            let first = hits.first().map_or(0, |h| h.0);
            start = first.saturating_sub(MAX_LINE_SHOWN / 4);
            end = (start + MAX_LINE_SHOWN).min(trimmed.len());
            while start > 0 && start < trimmed.len() && (trimmed[start] & 0xC0) == 0x80 {
                start -= 1;
            }
            while end < trimmed.len() && (trimmed[end] & 0xC0) == 0x80 {
                end += 1;
            }
        }
        let text = String::from_utf8_lossy(&trimmed[start..end]).into_owned();
        let ranges = hits
            .into_iter()
            .filter(|&(a, b)| a >= start && b <= end)
            .map(|(a, b)| (a - start, b - start))
            .collect();
        self.out.push(Match { line: m.line_number().unwrap_or(0), text, ranges });
        Ok(self.out.len() < self.max)
    }

    fn binary_data(&mut self, _s: &Searcher, _offset: u64) -> std::result::Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

fn build_matcher(o: &GrepOptions) -> Result<RegexMatcher> {
    if o.pattern.is_empty() {
        return Err(Error::Query("пустой шаблон".into()));
    }
    RegexMatcherBuilder::new()
        .case_insensitive(o.case_insensitive)
        .fixed_strings(!o.regex)
        .build(&o.pattern)
        .map_err(|e| Error::Query(format!("шаблон: {e}")))
}

fn searcher() -> Searcher {
    SearcherBuilder::new().line_number(true).binary_detection(BinaryDetection::quit(b'\0')).build()
}

/// Searches `files` in parallel. `on_file` is called (from worker threads) for every file that has matches.
pub fn grep(files: &[PathBuf], opts: &GrepOptions, progress: &Progress, on_file: &(dyn Fn(FileHits) + Sync)) -> Result<GrepStats> {
    let matcher = build_matcher(opts)?;
    progress.total.store(files.len() as u64, Relaxed);
    let (searched, with, total, binary, large, errors) =
        (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    files.par_iter().for_each_init(searcher, |searcher, path| {
        if progress.is_cancelled() {
            return;
        }
        progress.done.fetch_add(1, Relaxed);
        let lp = winapi::long_path(path);
        if let Some(max) = opts.max_filesize
            && std::fs::metadata(&lp).is_ok_and(|m| m.len() > max)
        {
            large.fetch_add(1, Relaxed);
            return;
        }
        let mut sink = Collect { matcher: &matcher, out: Vec::new(), max: opts.max_matches_per_file.max(1), binary: false };
        match searcher.search_path(&matcher, &lp, &mut sink) {
            Ok(()) => {
                searched.fetch_add(1, Relaxed);
                if sink.binary && sink.out.is_empty() {
                    binary.fetch_add(1, Relaxed);
                }
                if !sink.out.is_empty() {
                    with.fetch_add(1, Relaxed);
                    total.fetch_add(sink.out.len() as u64, Relaxed);
                    progress.items.fetch_add(sink.out.len() as u64, Relaxed);
                    on_file(FileHits { path: path.clone(), matches: sink.out });
                }
            }
            Err(_) => {
                errors.fetch_add(1, Relaxed);
            }
        }
    });
    if progress.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(GrepStats {
        files_searched: searched.load(Relaxed),
        files_with_matches: with.load(Relaxed),
        matches: total.load(Relaxed),
        binary_skipped: binary.load(Relaxed),
        too_large: large.load(Relaxed),
        errors: errors.load(Relaxed),
    })
}

/// Files selected by the name filters, from the index (no disk access).
pub fn candidates_from_index(index: &Index, filter: &str) -> Result<Vec<PathBuf>> {
    let opts = QueryOptions { fuzzy: false, limit: usize::MAX, ..Default::default() };
    let mut q = Query::parse(filter, &opts)?;
    q.files_only = true;
    let res = index.search(&q, &opts);
    Ok(res.hits.iter().map(|h| PathBuf::from(index.row(h).path)).collect())
}

/// Files below `root` selected by the name filters, by walking the folder.
pub fn candidates_from_walk(root: &Path, filter: &str, progress: &Progress) -> Result<Vec<PathBuf>> {
    let opts = QueryOptions { fuzzy: false, ..Default::default() };
    let mut q = Query::parse(filter, &opts)?;
    q.files_only = true;
    // a path: filter narrows the walk root instead of being evaluated per file
    let start = q.path.as_ref().map(PathBuf::from).filter(|p| p.is_absolute() && p.starts_with(root.components().next().map_or(Path::new(""), |c| Path::new(c.as_os_str())))).unwrap_or_else(|| root.to_path_buf());
    let out = std::sync::Mutex::new(Vec::new());
    fn walk(dir: &Path, q: &Query, out: &std::sync::Mutex<Vec<PathBuf>>, progress: &Progress) {
        if progress.is_cancelled() {
            return;
        }
        let mut subdirs = Vec::new();
        let mut here = Vec::new();
        let _ = winapi::list_dir(dir, |e: RawEntry| {
            use std::os::windows::ffi::OsStringExt;
            let name = std::ffi::OsString::from_wide(e.name);
            if e.is_reparse() {
                return;
            }
            if e.is_dir() {
                subdirs.push(dir.join(&name));
            } else if q.matches_meta(&name.to_string_lossy(), e.size, crate::index::filetime_to_unix(e.mtime), false) {
                here.push(dir.join(&name));
            }
        });
        progress.items.fetch_add(here.len() as u64, Relaxed);
        out.lock().unwrap().extend(here);
        subdirs.par_iter().for_each(|d| walk(d, q, out, progress));
    }
    walk(&start, &q, &out, progress);
    Ok(out.into_inner().unwrap())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::index::walk::scan_root;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("src/inner")).unwrap();
        fs::write(r.join("src/main.rs"), "fn main() {\n    println!(\"Hello Мир\");\n}\n// TODO: fix\n").unwrap();
        fs::write(r.join("src/inner/lib.rs"), "pub fn hello() {}\r\npub fn bye() {}\r\n").unwrap();
        fs::write(r.join("notes.txt"), "nothing here\ntodo list\n").unwrap();
        // binary file that contains the word after a NUL
        let mut bin = vec![0u8; 16];
        bin.extend_from_slice(b"hello inside binary");
        fs::write(r.join("blob.bin"), bin).unwrap();
        // UTF-16LE with BOM
        let mut u16: Vec<u8> = vec![0xFF, 0xFE];
        u16.extend("hello from utf16\n".encode_utf16().flat_map(|c| c.to_le_bytes()));
        fs::write(r.join("wide.txt"), u16).unwrap();
        tmp
    }

    fn run(files: &[PathBuf], pattern: &str, regex: bool) -> (Vec<FileHits>, GrepStats) {
        let hits = std::sync::Mutex::new(Vec::new());
        let stats = grep(
            files,
            &GrepOptions { pattern: pattern.into(), regex, ..Default::default() },
            &Progress::default(),
            &|h| hits.lock().unwrap().push(h),
        )
        .unwrap();
        let mut v = hits.into_inner().unwrap();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        (v, stats)
    }

    fn all_files(root: &Path) -> Vec<PathBuf> {
        candidates_from_walk(root, "", &Progress::default()).unwrap()
    }

    #[test]
    fn finds_lines_columns_and_ranges() {
        let tmp = fixture();
        let (hits, stats) = run(&all_files(tmp.path()), "hello", false);
        let by: Vec<_> = hits.iter().map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(by, ["lib.rs", "main.rs", "wide.txt"], "binary file skipped, utf-16 found");
        let main = hits.iter().find(|h| h.path.ends_with("main.rs")).unwrap();
        assert_eq!(main.matches.len(), 1);
        assert_eq!(main.matches[0].line, 2);
        assert_eq!(main.matches[0].text, "    println!(\"Hello Мир\");");
        assert_eq!(main.matches[0].ranges, vec![(14, 19)], "case-insensitive hit position");
        let lib = hits.iter().find(|h| h.path.ends_with("lib.rs")).unwrap();
        assert_eq!(lib.matches[0].text, "pub fn hello() {}", "CRLF trimmed");
        assert_eq!(stats.files_with_matches, 3);
        assert_eq!(stats.matches, 3);
    }

    #[test]
    fn regex_and_literal_modes_differ() {
        let tmp = fixture();
        let files = all_files(tmp.path());
        let (h, _) = run(&files, r"pub fn \w+\(\)", true);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].matches.len(), 2);
        let (h, _) = run(&files, r"pub fn \w+\(\)", false);
        assert!(h.is_empty(), "literal mode does not interpret the pattern");
        assert!(grep(&files, &GrepOptions { pattern: "(".into(), regex: true, ..Default::default() }, &Progress::default(), &|_| {}).is_err());
        assert!(grep(&files, &GrepOptions::default(), &Progress::default(), &|_| {}).is_err());
    }

    #[test]
    fn name_filters_choose_the_candidates() {
        let tmp = fixture();
        let rs = candidates_from_walk(tmp.path(), "ext:rs", &Progress::default()).unwrap();
        assert_eq!(rs.len(), 2);
        let (h, _) = run(&rs, "todo", false);
        assert_eq!(h.len(), 1, "notes.txt has 'todo' too but is filtered out");
        assert!(h[0].path.ends_with("main.rs"));
        let big = candidates_from_walk(tmp.path(), "size:>50", &Progress::default()).unwrap();
        assert!(big.iter().all(|p| fs::metadata(p).unwrap().len() > 50));

        // the same filters through the index
        let mut idx = Index::default();
        idx.set_volume(scan_root(tmp.path(), &Progress::default()).unwrap());
        let mut via_index = candidates_from_index(&idx, "ext:rs").unwrap();
        let mut via_walk = rs.clone();
        via_index.sort();
        via_walk.sort();
        assert_eq!(via_index, via_walk);
        let folder = tmp.path().join("src").join("inner");
        let scoped = candidates_from_index(&idx, &format!("path:\"{}\" ext:rs", folder.display())).unwrap();
        assert_eq!(scoped.len(), 1);
    }

    #[test]
    fn long_lines_are_windowed_without_breaking_utf8() {
        let tmp = tempfile::tempdir().unwrap();
        let line = format!("{}needle{}", "ж".repeat(900), "я".repeat(900));
        fs::write(tmp.path().join("min.js"), &line).unwrap();
        let (h, _) = run(&[tmp.path().join("min.js")], "needle", false);
        let m = &h[0].matches[0];
        assert!(m.text.len() <= MAX_LINE_SHOWN + 8);
        let (a, b) = m.ranges[0];
        assert_eq!(&m.text[a..b], "needle");
    }

    #[test]
    fn max_matches_and_size_cap() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("many.txt"), "x\n".repeat(1000)).unwrap();
        let hits = std::sync::Mutex::new(Vec::new());
        let stats = grep(
            &[tmp.path().join("many.txt")],
            &GrepOptions { pattern: "x".into(), max_matches_per_file: 5, ..Default::default() },
            &Progress::default(),
            &|h| hits.lock().unwrap().push(h),
        )
        .unwrap();
        assert_eq!(hits.lock().unwrap()[0].matches.len(), 5);
        assert_eq!(stats.matches, 5);
        let stats = grep(
            &[tmp.path().join("many.txt")],
            &GrepOptions { pattern: "x".into(), max_filesize: Some(100), ..Default::default() },
            &Progress::default(),
            &|_| panic!("too large"),
        )
        .unwrap();
        assert_eq!(stats.too_large, 1);
    }
}

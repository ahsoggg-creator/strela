//! Query language and the parallel search over the index.
//!
//! ```text
//! report 2024            every word must be part of the name (case-insensitive)
//! ext:mp4;mkv            extension(s)
//! size:>1GB  size:1MB..5MB   size filter (KB/MB/GB/TB are powers of 1024)
//! modified:<7d           modified within the last 7 days   (units: min h d w mo y)
//! modified:>1y           modified more than a year ago
//! modified:>2024-01-01   modified after a date
//! path:D:\Games          only below this folder (or: folders whose path contains the text)
//! ```
//! With the regex flag the free text is one regular expression applied to the file name. With the fuzzy flag,
//! names that contain no exact match are also tried with the `nucleo` fuzzy matcher when exact hits are few.

use std::time::{Duration, Instant};

use memchr::memmem;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rayon::prelude::*;
use regex::Regex;

use crate::error::{Error, Result};
use crate::index::{Index, NONE, VolumeIndex, fold};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortBy {
    #[default]
    Relevance,
    Size,
    Modified,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub regex: bool,
    pub fuzzy: bool,
    pub limit: usize,
    pub sort: SortBy,
    /// "Now" for relative dates; `None` = system time. Tests pin it.
    pub now: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options { regex: false, fuzzy: true, limit: 500, sort: SortBy::Relevance, now: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    /// Lowercase substrings that must all occur in the name.
    pub terms: Vec<String>,
    /// Original-case words for the fuzzy matcher.
    pub words: Vec<String>,
    pub exts: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Inclusive bounds on mtime (Unix seconds).
    pub min_mtime: Option<u32>,
    pub max_mtime: Option<u32>,
    pub path: Option<String>,
    pub regex: Option<Regex>,
    /// Skip folders (content search and cleanup only look at files).
    pub files_only: bool,
}

impl Query {
    /// Name / size / date filters applied to a directory listing entry (no index needed). The `path:` filter is
    /// not evaluated here — the caller restricts the walk to that folder instead.
    pub fn matches_meta(&self, name: &str, size: u64, mtime: u32, is_dir: bool) -> bool {
        if (self.files_only || self.has_size() || !self.exts.is_empty()) && is_dir {
            return false;
        }
        if self.min_size.is_some_and(|m| size < m) || self.max_size.is_some_and(|m| size > m) {
            return false;
        }
        if self.min_mtime.is_some_and(|m| mtime < m) || self.max_mtime.is_some_and(|m| mtime > m) {
            return false;
        }
        let lname = fold(name);
        if !self.exts.is_empty() {
            let ln = lname.as_bytes();
            let ok = self.exts.iter().any(|x| {
                ln.len() > x.len() && ln[ln.len() - x.len() - 1] == b'.' && &ln[ln.len() - x.len()..] == x.as_bytes()
            });
            if !ok {
                return false;
            }
        }
        if let Some(re) = &self.regex {
            return re.is_match(name);
        }
        self.terms.iter().all(|t| lname.contains(t.as_str()))
    }

    pub fn has_text(&self) -> bool {
        !self.terms.is_empty() || self.regex.is_some()
    }

    pub fn has_size(&self) -> bool {
        self.min_size.is_some() || self.max_size.is_some()
    }

    pub fn parse(input: &str, opts: &Options) -> Result<Query> {
        let now = opts.now.unwrap_or_else(unix_now);
        let mut q = Query::default();
        let mut free: Vec<String> = Vec::new();
        for tok in tokenize(input) {
            let lower = tok.to_lowercase();
            if let Some(v) = lower.strip_prefix("ext:") {
                q.exts.extend(
                    v.split([';', ',', '|'])
                        .map(|e| e.trim().trim_start_matches(['.', '*']).to_string())
                        .filter(|e| !e.is_empty()),
                );
            } else if let Some(v) = lower.strip_prefix("size:") {
                parse_size(v, &mut q)?;
            } else if let Some(v) = lower.strip_prefix("modified:") {
                parse_modified(v, now, &mut q)?;
            } else if lower.starts_with("path:") {
                let p = tok[5..].trim_matches('"').trim_end_matches('\\');
                if !p.is_empty() {
                    q.path = Some(fold(p).replace('/', "\\"));
                }
            } else {
                free.push(tok);
            }
        }
        if opts.regex {
            let text = free.join(" ");
            if !text.is_empty() {
                q.regex = Some(
                    regex::RegexBuilder::new(&text)
                        .case_insensitive(true)
                        .size_limit(8 << 20)
                        .build()
                        .map_err(|e| Error::Query(format!("регулярное выражение: {e}")))?,
                );
            }
        } else {
            for w in free {
                q.terms.push(fold(&w));
                q.words.push(w);
            }
        }
        Ok(q)
    }
}

fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

/// Splits on whitespace; double quotes group words (`"my file"`, also after a `path:` prefix).
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in s.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn parse_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.trim().replace(',', ".").parse().map_err(|_| Error::Query(format!("размер «{s}»")))?;
    let mul: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0f64.powi(4),
        u => return Err(Error::Query(format!("единица размера «{u}»"))),
    };
    Ok((n * mul).round().max(0.0) as u64)
}

fn parse_size(v: &str, q: &mut Query) -> Result<()> {
    if let Some((a, b)) = v.split_once("..") {
        if !a.is_empty() {
            q.min_size = Some(parse_bytes(a)?);
        }
        if !b.is_empty() {
            q.max_size = Some(parse_bytes(b)?);
        }
    } else if let Some(r) = v.strip_prefix(">=") {
        q.min_size = Some(parse_bytes(r)?);
    } else if let Some(r) = v.strip_prefix("<=") {
        q.max_size = Some(parse_bytes(r)?);
    } else if let Some(r) = v.strip_prefix('>') {
        q.min_size = Some(parse_bytes(r)?.saturating_add(1));
    } else if let Some(r) = v.strip_prefix('<') {
        q.max_size = Some(parse_bytes(r)?.saturating_sub(1));
    } else {
        let n = parse_bytes(v.strip_prefix('=').unwrap_or(v))?;
        q.min_size = Some(n);
        q.max_size = Some(n);
    }
    Ok(())
}

fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_alphabetic())?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let secs = match unit {
        "s" | "sec" => 1,
        "min" | "mi" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        "mo" => 30 * 86_400,
        "y" => 365 * 86_400,
        _ => return None,
    };
    Some(n * secs)
}

/// `YYYY-MM-DD` -> Unix seconds (UTC midnight).
fn parse_date(s: &str) -> Option<u32> {
    let mut it = s.split('-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return None;
    }
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400).clamp(0, u32::MAX as i64) as u32)
}

fn parse_modified(v: &str, now: u32, q: &mut Query) -> Result<()> {
    let bad = || Error::Query(format!("modified:{v}"));
    let (op, rest) = match v.as_bytes().first() {
        Some(b'<') => ('<', &v[1..]),
        Some(b'>') => ('>', &v[1..]),
        Some(b'=') => ('=', &v[1..]),
        _ => ('=', v),
    };
    if let Some(secs) = parse_duration(rest) {
        let t = now.saturating_sub(secs.min(u32::MAX as u64) as u32);
        match op {
            '<' => q.min_mtime = Some(t), // younger than N
            '>' => q.max_mtime = Some(t), // older than N
            _ => return Err(bad()),
        }
    } else if let Some(day) = parse_date(rest) {
        match op {
            '<' => q.max_mtime = Some(day.saturating_sub(1)),
            '>' => q.min_mtime = Some(day.saturating_add(86_400)),
            _ => {
                q.min_mtime = Some(day);
                q.max_mtime = Some(day.saturating_add(86_399));
            }
        }
    } else {
        return Err(bad());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub vol: u16,
    pub id: u32,
    pub score: i64,
}

#[derive(Debug)]
pub struct SearchResult {
    /// Number of matches (before truncating to `limit`).
    pub total: usize,
    pub hits: Vec<Hit>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u32,
}

impl Index {
    pub fn row(&self, hit: &Hit) -> Row {
        let v = &self.volumes[hit.vol as usize];
        let e = &v.entries[hit.id as usize];
        Row { path: v.path(hit.id), name: v.name(hit.id).to_string(), is_dir: e.is_dir(), size: e.size, mtime: e.mtime }
    }

    pub fn search(&self, q: &Query, opts: &Options) -> SearchResult {
        let t0 = Instant::now();
        let mut all: Vec<Hit> = Vec::new();
        let mut total = 0usize;
        for (vi, v) in self.volumes.iter().enumerate() {
            let hits = search_volume(v, vi as u16, q, opts);
            total += hits.len();
            all.extend(hits);
        }
        if q.has_text() && opts.fuzzy && opts.regex_off() && total < opts.limit && q.terms.iter().map(|t| t.chars().count()).sum::<usize>() >= 3 {
            let exact: std::collections::HashSet<(u16, u32)> = all.iter().map(|h| (h.vol, h.id)).collect();
            for (vi, v) in self.volumes.iter().enumerate() {
                let hits = fuzzy_volume(v, vi as u16, q, &exact);
                total += hits.len();
                all.extend(hits);
            }
        }
        let k = opts.limit.max(1);
        if all.len() > k {
            all.select_nth_unstable_by(k - 1, |a, b| b.score.cmp(&a.score));
            all.truncate(k);
        }
        all.sort_unstable_by(|a, b| b.score.cmp(&a.score).then(a.vol.cmp(&b.vol)).then(a.id.cmp(&b.id)));
        SearchResult { total, hits: all, elapsed: t0.elapsed() }
    }
}

impl Options {
    fn regex_off(&self) -> bool {
        !self.regex
    }
}

fn passes_filters(v: &VolumeIndex, id: u32, q: &Query, scope: Option<&[bool]>) -> bool {
    let e = &v.entries[id as usize];
    if e.is_deleted() || id == 0 || (q.files_only && e.is_dir()) {
        return false;
    }
    if q.has_size() {
        if e.is_dir() {
            return false;
        }
        if q.min_size.is_some_and(|m| e.size < m) || q.max_size.is_some_and(|m| e.size > m) {
            return false;
        }
    }
    if q.min_mtime.is_some_and(|m| e.mtime < m) || q.max_mtime.is_some_and(|m| e.mtime > m) {
        return false;
    }
    if !q.exts.is_empty() {
        if e.is_dir() {
            return false;
        }
        let ln = v.lname(id).as_bytes();
        let ok = q.exts.iter().any(|x| {
            ln.len() > x.len() && ln[ln.len() - x.len() - 1] == b'.' && &ln[ln.len() - x.len()..] == x.as_bytes()
        });
        if !ok {
            return false;
        }
    }
    if let Some(s) = scope
        && !s[id as usize]
    {
        return false;
    }
    true
}

/// Which entries lie inside the `path:` filter (the matching folders and everything below them).
fn path_scope(v: &VolumeIndex, needle: &str) -> Vec<bool> {
    let n = v.entries.len();
    let absolute = needle.len() >= 2 && needle.as_bytes()[1] == b':' || needle.starts_with("\\\\");
    let matches_dir: Vec<bool> = (0..n as u32)
        .into_par_iter()
        .map(|id| {
            let e = &v.entries[id as usize];
            if !e.is_dir() || e.is_deleted() {
                return false;
            }
            if absolute {
                // cheap reject: the last component of the needle must equal the folder name
                let last = needle.rsplit('\\').next().unwrap_or("");
                if id != 0 && v.lname(id) != last {
                    return false;
                }
                fold(&v.path(id)) == needle || (id == 0 && fold(&v.path(0)).trim_end_matches('\\') == needle)
            } else {
                fold(&v.path(id)).contains(needle)
            }
        })
        .collect();
    // memoised "self or any ancestor matches", parents may come later than children
    let mut state = vec![0u8; n]; // 0 unknown, 1 inside, 2 outside
    let mut stack: Vec<u32> = Vec::new();
    for start in 0..n as u32 {
        if state[start as usize] != 0 {
            continue;
        }
        let mut cur = start;
        let result;
        loop {
            if state[cur as usize] != 0 {
                result = state[cur as usize] == 1;
                break;
            }
            if matches_dir[cur as usize] {
                result = true;
                break;
            }
            stack.push(cur);
            let p = v.entries[cur as usize].parent;
            if p == NONE || stack.len() > 4096 {
                result = false;
                break;
            }
            cur = p;
        }
        let s = if result { 1 } else { 2 };
        for id in stack.drain(..) {
            state[id as usize] = s;
        }
        if matches_dir[start as usize] {
            state[start as usize] = 1;
        }
    }
    state.iter().map(|&s| s == 1).collect()
}

fn relevance(lname: &str, terms: &[String]) -> i64 {
    let first = &terms[0];
    let mut score = 10_000i64 - lname.len().min(2000) as i64;
    if let Some(pos) = lname.find(first.as_str()) {
        if pos == 0 {
            score += 1500;
        } else if !lname[..pos].chars().next_back().is_some_and(char::is_alphanumeric) {
            score += 700; // starts a word
        }
    }
    let stem = lname.rsplit_once('.').map_or(lname, |(s, _)| s);
    if terms.len() == 1 && stem == terms[0] {
        score += 3000;
    }
    score
}

fn search_volume(v: &VolumeIndex, vi: u16, q: &Query, opts: &Options) -> Vec<Hit> {
    let scope = q.path.as_deref().map(|p| path_scope(v, p));
    let scope = scope.as_deref();
    let finders: Vec<memmem::Finder> = q.terms.iter().map(|t| memmem::Finder::new(t.as_bytes()).into_owned()).collect();
    let n = v.entries.len();
    (0..n)
        .into_par_iter()
        .with_min_len(32 * 1024)
        .filter_map(|i| {
            let id = i as u32;
            if !passes_filters(v, id, q, scope) {
                return None;
            }
            let score = if let Some(re) = &q.regex {
                if !re.is_match(v.name(id)) {
                    return None;
                }
                10_000 - v.entries[i].name_len as i64
            } else if !finders.is_empty() {
                let ln = v.lname(id);
                if !finders.iter().all(|f| f.find(ln.as_bytes()).is_some()) {
                    return None;
                }
                relevance(ln, &q.terms)
            } else {
                0
            };
            let e = &v.entries[i];
            // Without any name text there is no relevance: lead with the biggest (size filter) or newest files.
            let sort = match opts.sort {
                SortBy::Relevance if !q.has_text() && q.has_size() => SortBy::Size,
                SortBy::Relevance if !q.has_text() => SortBy::Modified,
                s => s,
            };
            let score = match sort {
                SortBy::Relevance => score,
                SortBy::Size => e.size as i64,
                SortBy::Modified => e.mtime as i64,
            };
            Some(Hit { vol: vi, id, score: score + 1_000_000_000 })
        })
        .collect()
}

fn fuzzy_volume(v: &VolumeIndex, vi: u16, q: &Query, exact: &std::collections::HashSet<(u16, u32)>) -> Vec<Hit> {
    let scope = q.path.as_deref().map(|p| path_scope(v, p));
    let scope = scope.as_deref();
    let n = v.entries.len();
    let patterns: Vec<Pattern> =
        q.words.iter().map(|w| Pattern::new(w, CaseMatching::Ignore, Normalization::Smart, AtomKind::Fuzzy)).collect();
    (0..n)
        .into_par_iter()
        .with_min_len(32 * 1024)
        .map_init(
            || (Matcher::new(Config::DEFAULT), Vec::<char>::new()),
            |(m, buf), i| {
                let id = i as u32;
                if !passes_filters(v, id, q, scope) || exact.contains(&(vi, id)) {
                    return None;
                }
                let name = v.name(id);
                let hay = Utf32Str::new(name, buf);
                let mut total = 0u32;
                for p in &patterns {
                    total += p.score(hay, m)?;
                }
                Some(Hit { vol: vi, id, score: total as i64 })
            },
        )
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{F_DIR, VolumeKind};

    const NOW: u32 = 1_700_000_000;

    fn opts() -> Options {
        Options { now: Some(NOW), ..Options::default() }
    }

    fn build() -> Index {
        let mut v = VolumeIndex::new(VolumeKind::Walk, r"D:\");
        let day = 86_400;
        let games = v.add(None, 0, "Games", F_DIR, 0, NOW - day);
        let doom = v.add(None, games, "Doom", F_DIR, 0, NOW - day);
        let docs = v.add(None, 0, "Документы", F_DIR, 0, NOW - day);
        v.add(None, doom, "doom.exe", 0, 2_000_000, NOW - 3 * day);
        v.add(None, doom, "DOOM.WAD", 0, 12_000_000, NOW - 400 * day);
        v.add(None, games, "movie.mp4", 0, 3_000_000_000, NOW - 2 * day);
        v.add(None, games, "movie.mkv", 0, 900_000_000, NOW - 40 * day);
        v.add(None, docs, "Отчёт 2024.docx", 0, 50_000, NOW - day);
        v.add(None, docs, "report-final.pdf", 0, 700_000, NOW - 10 * day);
        v.add(None, 0, "doom_notes.txt", 0, 10, NOW - 100 * day);
        let mut i = Index::default();
        i.volumes.push(v);
        i
    }

    fn run(i: &Index, q: &str, o: &Options) -> Vec<String> {
        let query = Query::parse(q, o).unwrap();
        let r = i.search(&query, o);
        r.hits.iter().map(|h| i.row(h).path).collect()
    }

    #[test]
    fn substring_terms_are_case_insensitive_and_anded() {
        let i = build();
        let o = Options { fuzzy: false, ..opts() };
        let r = run(&i, "DOOM exe", &o);
        assert_eq!(r, vec![r"D:\Games\Doom\doom.exe"]);
        assert_eq!(run(&i, "отчёт", &o), vec![r"D:\Документы\Отчёт 2024.docx"]);
        assert_eq!(run(&i, "ОТЧЁТ 2024", &o).len(), 1);
        assert!(run(&i, "nothing-here", &o).is_empty());
    }

    #[test]
    fn ranking_prefers_exact_and_prefix_matches() {
        let i = build();
        let o = Options { fuzzy: false, ..opts() };
        let r = run(&i, "doom", &o);
        // folder "Doom" (exact stem) first, then names starting with doom, shorter first
        assert_eq!(r[0], r"D:\Games\Doom");
        assert!(r.iter().position(|p| p.ends_with("doom.exe")) < r.iter().position(|p| p.ends_with("doom_notes.txt")));
    }

    #[test]
    fn filters() {
        let i = build();
        let o = Options { fuzzy: false, ..opts() };
        assert_eq!(run(&i, "ext:mp4", &o), vec![r"D:\Games\movie.mp4"]);
        assert_eq!(run(&i, "ext:mp4;mkv", &o).len(), 2);
        assert_eq!(run(&i, "ext:.PDF", &o), vec![r"D:\Документы\report-final.pdf"]);
        assert_eq!(run(&i, "size:>1GB", &o), vec![r"D:\Games\movie.mp4"]);
        assert_eq!(run(&i, "size:1MB..5MB", &o), vec![r"D:\Games\Doom\doom.exe"]);
        assert_eq!(run(&i, "size:<100", &o), vec![r"D:\doom_notes.txt"]);
        assert_eq!(run(&i, "modified:<7d", &o).len(), 6, "{:?}", run(&i, "modified:<7d", &o));
        assert_eq!(run(&i, "modified:>1y", &o), vec![r"D:\Games\Doom\DOOM.WAD"]);
        assert_eq!(run(&i, "modified:<7d ext:mp4", &o), vec![r"D:\Games\movie.mp4"]);
        assert_eq!(run(&i, "ext:txt doom", &o), vec![r"D:\doom_notes.txt"]);
    }

    #[test]
    fn absolute_dates() {
        let i = build();
        let o = Options { fuzzy: false, ..opts() };
        // NOW is 2023-11-14; everything modified after 2023-11-10 is newer than 4 days
        let r = run(&i, "modified:>2023-11-10", &o);
        assert_eq!(r.len(), 6, "{r:?}");
        assert_eq!(parse_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_date("2000-03-01"), Some(951_868_800));
    }

    #[test]
    fn path_filter() {
        let i = build();
        let o = Options { fuzzy: false, ..opts() };
        let r = run(&i, r"path:D:\Games doom", &o);
        assert!(r.iter().all(|p| p.starts_with(r"D:\Games")), "{r:?}");
        assert!(!r.iter().any(|p| p.contains("notes")));
        let r = run(&i, r#"path:"d:\games\doom" ext:wad"#, &o);
        assert_eq!(r, vec![r"D:\Games\Doom\DOOM.WAD"]);
        assert_eq!(run(&i, r"path:D:\Документы", &o).len(), 3, "folder itself's children");
        // relative text: folders whose path contains it
        assert!(run(&i, r"path:games ext:mp4", &o).len() == 1);
        assert!(run(&i, r"path:D:\Nope", &o).is_empty());
    }

    #[test]
    fn regex_mode() {
        let i = build();
        let o = Options { regex: true, ..opts() };
        assert_eq!(run(&i, r"^doom\.(exe|wad)$", &o).len(), 2);
        assert_eq!(run(&i, r"\d{4}\.docx$ size:<100KB", &o).len(), 1);
        assert!(Query::parse("(", &o).is_err());
    }

    #[test]
    fn fuzzy_finds_subsequences_after_exact_hits() {
        let i = build();
        let o = opts();
        let r = run(&i, "rptfnl", &o);
        assert_eq!(r, vec![r"D:\Документы\report-final.pdf"]);
        // exact hits come before fuzzy ones
        let r = run(&i, "doom", &o);
        assert_eq!(r[0], r"D:\Games\Doom");
        // fuzzy is off in regex mode and for very short queries
        assert!(run(&i, "rf", &o).is_empty());
    }

    #[test]
    fn sort_by_size_and_limit() {
        let i = build();
        let o = Options { fuzzy: false, limit: 2, sort: SortBy::Size, ..opts() };
        let q = Query::parse("ext:mp4;mkv;wad;exe", &o).unwrap();
        let r = i.search(&q, &o);
        assert_eq!(r.total, 4);
        let rows: Vec<_> = r.hits.iter().map(|h| i.row(h).name).collect();
        assert_eq!(rows, vec!["movie.mp4", "movie.mkv"]);
    }

    #[test]
    fn bad_filters_are_errors() {
        let o = opts();
        assert!(Query::parse("size:>abc", &o).is_err());
        assert!(Query::parse("size:>5XB", &o).is_err());
        assert!(Query::parse("modified:<7q", &o).is_err());
    }

    #[test]
    fn deleted_entries_are_not_found() {
        let mut i = build();
        let id = (0..i.volumes[0].entries.len() as u32).find(|&x| i.volumes[0].name(x) == "movie.mp4").unwrap();
        i.volumes[0].remove(id);
        assert!(run(&i, "movie", &Options { fuzzy: false, ..opts() }).len() == 1);
    }
}

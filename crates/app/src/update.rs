//! Updates from GitHub Releases: ask the API for the latest release, download `Strela.exe` next to the running one,
//! check it against the release's `SHA256SUMS.txt`, swap the files (a running executable can be renamed, not
//! overwritten) and start the new copy. One request to `api.github.com` per check; nothing else is sent.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::state::Shared;

pub const REPO: &str = "ahsoggg-creator/strela";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub notes: String,
    pub page: String,
    pub exe_url: String,
    pub exe_size: u64,
    pub sums_url: String,
}

#[derive(Clone, Debug, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Installing,
    Failed(String),
}

pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = it.next().map_or(Some(0), |p| p.parse().ok())?;
    it.next().is_none().then_some((major, minor, patch))
}

pub fn is_newer(remote: &str, local: &str) -> bool {
    matches!((parse_version(remote), parse_version(local)), (Some(r), Some(l)) if r > l)
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// `Some` when the release is newer than `current` and carries everything an update needs.
pub fn parse_release(json: &[u8], current: &str) -> Result<Option<Release>, String> {
    let r: ApiRelease = serde_json::from_slice(json).map_err(|e| format!("ответ GitHub не разобран: {e}"))?;
    if r.draft || r.prerelease || !is_newer(&r.tag_name, current) {
        return Ok(None);
    }
    let find = |n: &str| r.assets.iter().find(|a| a.name.eq_ignore_ascii_case(n));
    let (Some(exe), Some(sums)) = (find("Strela.exe"), find("SHA256SUMS.txt")) else {
        return Err(format!("в релизе {} нет Strela.exe и SHA256SUMS.txt", r.tag_name));
    };
    Ok(Some(Release {
        version: r.tag_name.trim_start_matches(['v', 'V']).to_string(),
        tag: r.tag_name,
        notes: r.body,
        page: r.html_url,
        exe_url: exe.browser_download_url.clone(),
        exe_size: exe.size,
        sums_url: sums.browser_download_url.clone(),
    }))
}

/// The hash of `name` from a `sha256sum`-style file (`<hex>  <name>`).
pub fn hash_from_sums(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (h, n) = l.trim().split_once(char::is_whitespace)?;
        (n.trim().trim_start_matches('*').eq_ignore_ascii_case(name) && h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
    })
}

fn agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(secs))).build().into()
}

fn user_agent() -> String {
    format!("Strela/{CURRENT} (+https://github.com/{REPO})")
}

/// Asks GitHub for the latest release. `Ok(None)` — already up to date.
pub fn check(current: &str) -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut resp = agent(20)
        .get(&url)
        .header("User-Agent", user_agent())
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("нет связи с GitHub: {e}"))?;
    let body = resp.body_mut().read_to_vec().map_err(|e| format!("ответ GitHub не получен: {e}"))?;
    parse_release(&body, current)
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut n = exe.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    n.push(suffix);
    exe.with_file_name(n)
}

/// Puts `new` in place of `exe`, keeping the old file as `<name>.old`; rolls back if the second rename fails.
pub fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    let old = sibling(exe, ".old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("не удалось переименовать {}: {e}", exe.display()))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("не удалось поставить новую версию: {e}"));
    }
    Ok(())
}

/// Removes the `.old` copy that an earlier update left next to the running executable.
pub fn cleanup_old() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(sibling(&exe, ".old"));
        let _ = std::fs::remove_file(sibling(&exe, ".new"));
    }
}

/// Downloads, verifies and swaps in the release; returns the path of the new executable (same as the running one).
pub fn install(rel: &Release, progress: &dyn Fn(u64, u64), cancel: &AtomicBool) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let sums = agent(30).get(&rel.sums_url).header("User-Agent", user_agent()).call().map_err(|e| format!("SHA256SUMS.txt не скачан: {e}"))?.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let want = hash_from_sums(&sums, "Strela.exe").ok_or("в SHA256SUMS.txt нет суммы для Strela.exe")?;
    let new = sibling(&exe, ".new");
    let result = (|| -> Result<(), String> {
        let resp = agent(600).get(&rel.exe_url).header("User-Agent", user_agent()).call().map_err(|e| format!("Strela.exe не скачан: {e}"))?;
        let total = rel.exe_size.max(resp.body().content_length().unwrap_or(0));
        let mut reader = resp.into_body().into_reader();
        let mut out = std::fs::File::create(&new).map_err(|e| format!("не удалось создать {}: {e} (папка программы недоступна для записи — запустите Strela от администратора)", new.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            if cancel.load(Relaxed) {
                return Err("отменено".into());
            }
            let n = reader.read(&mut buf).map_err(|e| format!("обрыв загрузки: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            done += n as u64;
            progress(done, total);
        }
        out.flush().map_err(|e| e.to_string())?;
        drop(out);
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != want {
            return Err(format!("контрольная сумма не совпала (ожидалась {}…, получена {}…): файл повреждён, установка отменена", &want[..12], &got[..12]));
        }
        if done < 1_000_000 {
            return Err("скачанный файл подозрительно мал".into());
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    swap(&exe, &new).inspect_err(|_| {
        let _ = std::fs::remove_file(&new);
    })?;
    Ok(exe)
}

/// Starts the new executable; it waits for this process to exit before taking the single-instance lock.
pub fn restart(exe: &Path) {
    let _ = std::process::Command::new(exe).arg("--wait-pid").arg(std::process::id().to_string()).spawn();
}

/// Blocks until process `pid` has exited (at most `secs`).
pub fn wait_for_exit(pid: u32, secs: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    // SAFETY: a plain open / wait / close on a process handle.
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, secs * 1000);
            CloseHandle(h);
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// One check, recorded in the shared state and in the settings.
pub fn check_now(shared: &Arc<Shared>) {
    *shared.update.lock().unwrap() = UpdateState::Checking;
    shared.repaint();
    let r = check(CURRENT);
    {
        let mut s = shared.settings.write().unwrap();
        s.last_update_check = now_unix();
        s.save();
    }
    let state = match r {
        Ok(Some(rel)) => {
            let first_time = !matches!(&*shared.update.lock().unwrap(), UpdateState::Available(x) if x.version == rel.version);
            if first_time {
                shared.notify("Strela", &format!("Доступна версия {} — обновить можно в окне программы", rel.version), false);
            }
            UpdateState::Available(rel)
        }
        Ok(None) => UpdateState::UpToDate,
        Err(e) => UpdateState::Failed(e),
    };
    *shared.update.lock().unwrap() = state;
    shared.repaint();
}

/// Background checker: a little after start, then about every six hours while the program runs.
pub fn start_background(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("strela-update".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(25));
            loop {
                if shared.quitting.load(Relaxed) {
                    return;
                }
                let s = shared.settings();
                if s.check_updates && now_unix().saturating_sub(s.last_update_check) > 20 * 3600 {
                    check_now(&shared);
                }
                std::thread::sleep(Duration::from_secs(3600));
            }
        })
        .expect("spawn update thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.10"), Some((1, 10, 0)));
        assert_eq!(parse_version("0.3.0-beta.1"), Some((0, 3, 0)));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert!(is_newer("v0.10.0", "0.9.9"), "numeric, not textual, comparison");
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("nightly", "0.2.0"));
    }

    fn sample(tag: &str, with_assets: bool) -> String {
        let assets = if with_assets {
            r#"[{"name":"Strela.exe","browser_download_url":"https://x/Strela.exe","size":11433472},
                {"name":"SHA256SUMS.txt","browser_download_url":"https://x/SHA256SUMS.txt","size":200},
                {"name":"strela-cli.exe","browser_download_url":"https://x/c.exe","size":1}]"#
        } else {
            "[]"
        };
        format!(r#"{{"tag_name":"{tag}","body":"notes","html_url":"https://github.com/r","draft":false,"prerelease":false,"assets":{assets}}}"#)
    }

    #[test]
    fn release_parsing() {
        let r = parse_release(sample("v0.3.0", true).as_bytes(), "0.2.0").unwrap().unwrap();
        assert_eq!((r.version.as_str(), r.exe_size, r.exe_url.as_str()), ("0.3.0", 11433472, "https://x/Strela.exe"));
        assert_eq!(parse_release(sample("v0.2.0", true).as_bytes(), "0.2.0").unwrap(), None, "same version");
        assert_eq!(parse_release(sample("v0.1.0", true).as_bytes(), "0.2.0").unwrap(), None, "older");
        assert!(parse_release(sample("v0.3.0", false).as_bytes(), "0.2.0").is_err(), "newer but without files");
        assert!(parse_release(b"{}", "0.2.0").is_err());
        let pre = sample("v0.3.0", true).replace("\"prerelease\":false", "\"prerelease\":true");
        assert_eq!(parse_release(pre.as_bytes(), "0.2.0").unwrap(), None, "prereleases are ignored");
    }

    #[test]
    fn sums() {
        let h = "a".repeat(64);
        let t = format!("{h}  Strela-Setup.exe\n{}  Strela.exe\n{h} *strela-cli.exe\n", "B".repeat(64));
        assert_eq!(hash_from_sums(&t, "Strela.exe").unwrap(), "b".repeat(64));
        assert_eq!(hash_from_sums(&t, "strela-cli.exe").unwrap(), h);
        assert!(hash_from_sums(&t, "other.exe").is_none());
        assert!(hash_from_sums("zz  Strela.exe", "Strela.exe").is_none(), "not a hash");
    }

    #[test]
    fn swap_keeps_old_and_rolls_back() {
        let d = tempfile::tempdir().unwrap();
        let (exe, new) = (d.path().join("Strela.exe"), d.path().join("Strela.exe.new"));
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(d.path().join("Strela.exe.old")).unwrap(), b"old");
        assert!(!new.exists());
        // a missing new file: the current one must be restored
        let missing = d.path().join("nope");
        assert!(swap(&exe, &missing).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new", "rolled back");
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Talks to GitHub: `cargo test -p strela-app -- --ignored update::network_tests`.
    #[test]
    #[ignore = "needs network"]
    fn latest_release_is_found_for_an_old_version() {
        let r = check("0.0.1").expect("api reachable").expect("a release newer than 0.0.1");
        assert!(r.exe_url.ends_with("Strela.exe") && r.sums_url.ends_with("SHA256SUMS.txt"), "{r:?}");
        assert!(check("999.0.0").expect("api reachable").is_none());
    }
}

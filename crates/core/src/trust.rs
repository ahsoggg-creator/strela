//! Digital signatures (`WinVerifyTrust`, embedded and catalog) checked on background threads and cached, plus the
//! "runs from a place programs should not run from" test. Used by the startup and process tabs.

#![allow(clippy::field_reassign_with_default)]

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Security::Cryptography::Catalog::{
    CATALOG_INFO, CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2, CryptCATAdminEnumCatalogFromHash, CryptCATAdminReleaseCatalogContext,
    CryptCATAdminReleaseContext, CryptCATCatalogInfoFromContext,
};
use windows_sys::Win32::Security::WinTrust::{
    WINTRUST_CATALOG_INFO, WINTRUST_DATA, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_CATALOG, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE,
    WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrust,
};
use windows_sys::core::GUID;

use crate::winapi::{wide, wide_str};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// A valid signature (embedded or from a Windows catalog).
    Signed,
    Unsigned,
    /// Signed, but the signature does not hold (file changed after signing, untrusted or broken chain).
    Invalid,
    /// The file could not be read.
    Unknown,
}

const TRUST_E_NOSIGNATURE: i32 = 0x800B0100_u32 as i32;
const TRUST_E_PROVIDER_UNKNOWN: i32 = 0x800B0001_u32 as i32;
const TRUST_E_SUBJECT_FORM_UNKNOWN: i32 = 0x800B0003_u32 as i32;
const TRUST_E_FILE_NOT_FOUND: i32 = 0x80092004_u32 as i32;
const CRYPT_E_FILE_ERROR: i32 = 0x80092003_u32 as i32;

const GENERIC_VERIFY_V2: GUID = GUID { data1: 0x00AAC56B, data2: 0xCD44, data3: 0x11D0, data4: [0x8C, 0xC2, 0x00, 0xC0, 0x4F, 0xC2, 0x95, 0xEE] };

fn run_verify(data: &mut WINTRUST_DATA) -> i32 {
    let mut action = GENERIC_VERIFY_V2;
    data.cbStruct = size_of::<WINTRUST_DATA>() as u32;
    data.dwUIChoice = WTD_UI_NONE;
    data.fdwRevocationChecks = WTD_REVOKE_NONE;
    data.dwStateAction = WTD_STATEACTION_VERIFY;
    data.dwProvFlags = WTD_REVOCATION_CHECK_NONE | WTD_CACHE_ONLY_URL_RETRIEVAL;
    // SAFETY: `data` and everything it points to outlive both calls; the second call releases the state of the first.
    let r = unsafe { WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, data as *mut WINTRUST_DATA as *mut c_void) };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe { WinVerifyTrust(INVALID_HANDLE_VALUE, &mut action, data as *mut WINTRUST_DATA as *mut c_void) };
    r
}

fn verify_embedded(path: &Path) -> i32 {
    let w = wide(path);
    // SAFETY: all-zero is a valid (empty) WINTRUST_FILE_INFO.
    let mut fi: WINTRUST_FILE_INFO = unsafe { std::mem::zeroed() };
    fi.cbStruct = size_of::<WINTRUST_FILE_INFO>() as u32;
    fi.pcwszFilePath = w.as_ptr();
    let mut data = WINTRUST_DATA::default();
    data.dwUnionChoice = WTD_CHOICE_FILE;
    data.Anonymous.pFile = &mut fi;
    run_verify(&mut data)
}

/// Windows' own files are signed through catalogs rather than embedded signatures.
fn verify_catalog(path: &Path) -> Option<i32> {
    let file = std::fs::File::open(path).ok()?;
    let h = file.as_raw_handle();
    for alg in [Some("SHA256"), None] {
        let walg = alg.map(wide_str);
        let mut admin: isize = 0;
        // SAFETY: plain out-parameters; every context acquired below is released before the loop turns.
        unsafe {
            if CryptCATAdminAcquireContext2(&mut admin, std::ptr::null(), walg.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()), std::ptr::null(), 0) == 0 {
                continue;
            }
            let mut len = 0u32;
            CryptCATAdminCalcHashFromFileHandle2(admin, h, &mut len, std::ptr::null_mut(), 0);
            let mut hash = vec![0u8; len as usize];
            let mut result = None;
            if len > 0 && CryptCATAdminCalcHashFromFileHandle2(admin, h, &mut len, hash.as_mut_ptr(), 0) != 0 {
                let cat = CryptCATAdminEnumCatalogFromHash(admin, hash.as_ptr(), len, 0, std::ptr::null_mut());
                if cat != 0 {
                    let mut info = CATALOG_INFO::default();
                    info.cbStruct = size_of::<CATALOG_INFO>() as u32;
                    if CryptCATCatalogInfoFromContext(cat, &mut info, 0) != 0 {
                        let tag: Vec<u16> = hash.iter().flat_map(|b| format!("{b:02X}").encode_utf16().collect::<Vec<_>>()).chain(std::iter::once(0)).collect();
                        let member = wide(path);
                        let mut ci: WINTRUST_CATALOG_INFO = std::mem::zeroed();
                        ci.cbStruct = size_of::<WINTRUST_CATALOG_INFO>() as u32;
                        ci.pcwszCatalogFilePath = info.wszCatalogFile.as_ptr();
                        ci.pcwszMemberTag = tag.as_ptr();
                        ci.pcwszMemberFilePath = member.as_ptr();
                        ci.hMemberFile = h;
                        ci.pbCalculatedFileHash = hash.as_mut_ptr();
                        ci.cbCalculatedFileHash = len;
                        ci.hCatAdmin = admin;
                        let mut data = WINTRUST_DATA::default();
                        data.dwUnionChoice = WTD_CHOICE_CATALOG;
                        data.Anonymous.pCatalog = &mut ci;
                        result = Some(run_verify(&mut data));
                    }
                    CryptCATAdminReleaseCatalogContext(admin, cat, 0);
                }
            }
            CryptCATAdminReleaseContext(admin, 0);
            if result.is_some() {
                return result;
            }
        }
    }
    None
}

/// Checks one file now (can take tens of milliseconds; use [`lookup`] from a UI).
pub fn verify(path: &Path) -> Trust {
    match verify_embedded(path) {
        0 => Trust::Signed,
        TRUST_E_FILE_NOT_FOUND | CRYPT_E_FILE_ERROR => Trust::Unknown,
        TRUST_E_NOSIGNATURE | TRUST_E_PROVIDER_UNKNOWN | TRUST_E_SUBJECT_FORM_UNKNOWN => match verify_catalog(path) {
            Some(0) => Trust::Signed,
            Some(_) => Trust::Invalid,
            None => Trust::Unsigned,
        },
        _ => Trust::Invalid,
    }
}

// ---------------------------------------------------------------------------------------------------------
// Background cache

struct State {
    done: Mutex<HashMap<String, Trust>>,
    queued: Mutex<HashSet<String>>,
    tx: Sender<(String, PathBuf)>,
    generation: AtomicU64,
}

fn state() -> &'static State {
    static S: OnceLock<State> = OnceLock::new();
    S.get_or_init(|| {
        let (tx, rx) = channel::<(String, PathBuf)>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..2 {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("strela-trust-{i}")).spawn(move || {
                loop {
                    let job = rx.lock().unwrap().recv();
                    let Ok((key, path)) = job else { return };
                    let t = verify(&path);
                    let s = state();
                    s.done.lock().unwrap().insert(key.clone(), t);
                    s.queued.lock().unwrap().remove(&key);
                    s.generation.fetch_add(1, Relaxed);
                }
            });
        }
        State { done: Mutex::new(HashMap::new()), queued: Mutex::new(HashSet::new()), tx, generation: AtomicU64::new(0) }
    })
}

/// The cached verdict, or `None` while the file waits for a background check (queued on first ask).
pub fn lookup(path: &Path) -> Option<Trust> {
    let s = state();
    let key = crate::guard::normalize(path);
    if let Some(t) = s.done.lock().unwrap().get(&key) {
        return Some(*t);
    }
    if s.queued.lock().unwrap().insert(key.clone()) {
        let _ = s.tx.send((key, path.to_path_buf()));
    }
    None
}

/// Grows whenever a background check finishes — a UI can repaint when it changes.
pub fn generation() -> u64 {
    state().generation.load(Relaxed)
}

/// Files still waiting for a verdict.
pub fn pending() -> usize {
    state().queued.lock().unwrap().len()
}

// ---------------------------------------------------------------------------------------------------------
// Location

/// Where the file lives, when that is a place programs normally do not run from.
pub fn risky_location(path: &Path) -> Option<&'static str> {
    let n = crate::guard::normalize(path);
    let tmp = std::env::var("TEMP").map(|t| crate::guard::normalize(Path::new(&t))).unwrap_or_default();
    if (!tmp.is_empty() && n.starts_with(&tmp)) || n.contains(r"\appdata\local\temp\") || n.contains(r"\windows\temp\") {
        Some("временная папка")
    } else if n.contains(r"\downloads\") {
        Some("«Загрузки»")
    } else if n.contains(r"\appdata\") && !n.contains(r"\appdata\local\programs\") && !n.contains(r"\appdata\local\microsoft\") && !n.contains(r"\appdata\local\packages\") {
        Some("AppData")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_files_are_signed_through_catalogs() {
        let dir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        assert_eq!(verify(&Path::new(&dir).join(r"System32\kernel32.dll")), Trust::Signed);
        assert_eq!(verify(&Path::new(&dir).join(r"System32\notepad.exe")), Trust::Signed);
    }

    #[test]
    fn plain_file_is_unsigned() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.exe");
        std::fs::write(&f, b"MZ not really").unwrap();
        assert!(matches!(verify(&f), Trust::Unsigned | Trust::Invalid));
        assert_eq!(verify(&tmp.path().join("missing.exe")), Trust::Unknown);
    }

    #[test]
    fn risky_places() {
        assert!(risky_location(Path::new(r"C:\Users\a\Downloads\x.exe")).is_some());
        assert!(risky_location(Path::new(r"C:\Users\a\AppData\Roaming\x\y.exe")).is_some());
        assert!(risky_location(Path::new(r"C:\Program Files\x\y.exe")).is_none());
        assert!(risky_location(Path::new(r"C:\Users\a\AppData\Local\Programs\x\y.exe")).is_none());
    }
}

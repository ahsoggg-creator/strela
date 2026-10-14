//! A thin registry layer shared by the program uninstaller, the startup manager and the leftover cleaner.

use std::io;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, KEY_WRITE, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ,
    REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};

use crate::winapi::wide_str;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Hive {
    Machine,
    User,
}

impl Hive {
    fn raw(self) -> HKEY {
        match self {
            Hive::Machine => HKEY_LOCAL_MACHINE,
            Hive::User => HKEY_CURRENT_USER,
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Hive::Machine => "HKLM",
            Hive::User => "HKCU",
        }
    }
}

/// Which registry view a key lives in (64-bit apps on 64-bit Windows see `Native`; 32-bit apps' data is `Wow32`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum View {
    Native,
    Wow32,
}

impl View {
    fn flag(self) -> u32 {
        match self {
            View::Native => KEY_WOW64_64KEY,
            View::Wow32 => KEY_WOW64_32KEY,
        }
    }
}

/// A full key address: where it is and in which view.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyPath {
    pub hive: Hive,
    pub view: View,
    pub path: String,
}

impl KeyPath {
    pub fn new(hive: Hive, view: View, path: impl Into<String>) -> KeyPath {
        KeyPath { hive, view, path: path.into() }
    }

    pub fn display(&self) -> String {
        let wow = if self.view == View::Wow32 { " (32-бит)" } else { "" };
        format!("{}\\{}{wow}", self.hive.short(), self.path)
    }

    pub fn child(&self, name: &str) -> KeyPath {
        KeyPath { hive: self.hive, view: self.view, path: format!("{}\\{name}", self.path) }
    }

    pub fn open(&self, write: bool) -> Option<RegKey> {
        RegKey::open(self.hive, self.view, &self.path, write)
    }

    pub fn exists(&self) -> bool {
        self.open(false).is_some()
    }
}

pub struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegOpenKeyExW / RegCreateKeyExW and is closed exactly once.
        unsafe { RegCloseKey(self.0) };
    }
}

#[derive(Clone, Debug)]
pub struct Value {
    pub name: String,
    pub kind: u32,
    pub data: Vec<u8>,
}

impl Value {
    /// The text of REG_SZ / REG_EXPAND_SZ (environment variables expanded for the latter).
    pub fn as_string(&self) -> Option<String> {
        if self.kind != REG_SZ && self.kind != REG_EXPAND_SZ {
            return None;
        }
        let w: Vec<u16> = self.data.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let s = String::from_utf16_lossy(&w);
        let s = s.split('\0').next().unwrap_or("").to_string();
        Some(if self.kind == REG_EXPAND_SZ { expand_env(&s) } else { s })
    }

    pub fn as_dword(&self) -> Option<u32> {
        if self.kind == REG_DWORD && self.data.len() >= 4 { Some(u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]])) } else { None }
    }

    pub fn is_text(&self) -> bool {
        self.kind == REG_SZ || self.kind == REG_EXPAND_SZ
    }

    pub fn is_multi(&self) -> bool {
        self.kind == REG_MULTI_SZ
    }
}

/// `%SystemRoot%\x` -> `C:\Windows\x`.
pub fn expand_env(s: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    if !s.contains('%') {
        return s.to_string();
    }
    let w = wide_str(s);
    // SAFETY: `w` is NUL-terminated; the first call only asks for the required size (in characters, with the NUL).
    let n = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), std::ptr::null_mut(), 0) };
    if n == 0 {
        return s.to_string();
    }
    let mut buf = vec![0u16; n as usize];
    // SAFETY: the buffer holds `n` characters.
    let m = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), buf.as_mut_ptr(), n) };
    if m == 0 || m > n {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..(m as usize).saturating_sub(1)])
}

fn err(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

impl RegKey {
    pub fn open(hive: Hive, view: View, path: &str, write: bool) -> Option<RegKey> {
        let p = wide_str(path);
        let mut h: HKEY = std::ptr::null_mut();
        let access = if write { KEY_READ | KEY_WRITE } else { KEY_READ } | view.flag();
        // SAFETY: `p` is NUL-terminated; the out-pointer references a live local.
        let rc = unsafe { RegOpenKeyExW(hive.raw(), p.as_ptr(), 0, access, &mut h) };
        (rc == ERROR_SUCCESS).then_some(RegKey(h))
    }

    pub fn create(hive: Hive, view: View, path: &str) -> io::Result<RegKey> {
        let p = wide_str(path);
        let mut h: HKEY = std::ptr::null_mut();
        // SAFETY: as above.
        let rc = unsafe {
            RegCreateKeyExW(hive.raw(), p.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_READ | KEY_WRITE | view.flag(), std::ptr::null(), &mut h, std::ptr::null_mut())
        };
        if rc == ERROR_SUCCESS { Ok(RegKey(h)) } else { Err(err(rc)) }
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = vec![0u16; 512];
        for i in 0.. {
            let mut len = buf.len() as u32;
            // SAFETY: `buf` has `len` characters; the optional out-parameters are null.
            let rc = unsafe { RegEnumKeyExW(self.0, i, buf.as_mut_ptr(), &mut len, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
            match rc {
                ERROR_SUCCESS => out.push(String::from_utf16_lossy(&buf[..len as usize])),
                ERROR_NO_MORE_ITEMS => break,
                ERROR_MORE_DATA => buf.resize(buf.len() * 2, 0),
                _ => break,
            }
            if i > 1_000_000 {
                break;
            }
        }
        out
    }

    pub fn values(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let mut name = vec![0u16; 16384];
        let mut data = vec![0u8; 4096];
        let mut i = 0u32;
        loop {
            let mut nlen = name.len() as u32;
            let mut dlen = data.len() as u32;
            let mut kind = 0u32;
            // SAFETY: both buffers are as long as the lengths passed.
            let rc = unsafe { RegEnumValueW(self.0, i, name.as_mut_ptr(), &mut nlen, std::ptr::null_mut(), &mut kind, data.as_mut_ptr(), &mut dlen) };
            match rc {
                ERROR_SUCCESS => {
                    out.push(Value { name: String::from_utf16_lossy(&name[..nlen as usize]), kind, data: data[..dlen as usize].to_vec() });
                    i += 1;
                }
                ERROR_MORE_DATA => data.resize((dlen as usize).max(data.len() * 2), 0), // retry the same index
                _ => break,
            }
        }
        out
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let n = wide_str(name);
        let mut len = 0u32;
        let mut kind = 0u32;
        // SAFETY: the first call only asks for the size and type.
        let rc = unsafe { RegQueryValueExW(self.0, n.as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut len) };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let mut data = vec![0u8; len as usize + 2];
        let mut len2 = data.len() as u32;
        // SAFETY: the buffer has `len2` bytes.
        let rc = unsafe { RegQueryValueExW(self.0, n.as_ptr(), std::ptr::null(), &mut kind, data.as_mut_ptr(), &mut len2) };
        if rc != ERROR_SUCCESS {
            return None;
        }
        data.truncate(len2 as usize);
        Some(Value { name: name.to_string(), kind, data })
    }

    pub fn get_string(&self, name: &str) -> Option<String> {
        self.get(name).and_then(|v| v.as_string())
    }

    pub fn get_dword(&self, name: &str) -> Option<u32> {
        self.get(name).and_then(|v| v.as_dword())
    }

    pub fn set_raw(&self, name: &str, kind: u32, data: &[u8]) -> io::Result<()> {
        let n = wide_str(name);
        // SAFETY: the data pointer and length describe the slice.
        let rc = unsafe { RegSetValueExW(self.0, n.as_ptr(), 0, kind, data.as_ptr(), data.len() as u32) };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(err(rc)) }
    }

    pub fn set_binary(&self, name: &str, data: &[u8]) -> io::Result<()> {
        self.set_raw(name, REG_BINARY, data)
    }

    pub fn delete_value(&self, name: &str) -> io::Result<()> {
        let n = wide_str(name);
        // SAFETY: NUL-terminated name.
        let rc = unsafe { RegDeleteValueW(self.0, n.as_ptr()) };
        if rc == ERROR_SUCCESS { Ok(()) } else { Err(err(rc)) }
    }
}

/// Deletes a key with everything below it. A missing key is not an error.
pub fn delete_tree(k: &KeyPath) -> io::Result<()> {
    let (parent, leaf) = match k.path.rsplit_once('\\') {
        Some((p, l)) => (p.to_string(), l.to_string()),
        None => (String::new(), k.path.clone()),
    };
    if leaf.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "пустой путь ключа"));
    }
    let base = if parent.is_empty() {
        // deleting a root-level key: open the hive itself with the right view
        RegKey::open(k.hive, k.view, "", true)
    } else {
        RegKey::open(k.hive, k.view, &parent, true)
    };
    let Some(base) = base else {
        return if k.exists() { Err(io::Error::from(io::ErrorKind::PermissionDenied)) } else { Ok(()) };
    };
    let l = wide_str(&leaf);
    // SAFETY: NUL-terminated sub-key name relative to an open handle.
    let rc = unsafe { RegDeleteTreeW(base.0, l.as_ptr()) };
    match rc {
        ERROR_SUCCESS | 2 => Ok(()), // 2 = ERROR_FILE_NOT_FOUND
        c => Err(err(c)),
    }
}

/// Saves a key to a `.reg` file with the system's `reg.exe` (the user can double-click it to restore).
pub fn export(k: &KeyPath, file: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let root = match k.hive {
        Hive::Machine => "HKLM",
        Hive::User => "HKCU",
    };
    let arch = if k.view == View::Wow32 { "/reg:32" } else { "/reg:64" };
    let st = std::process::Command::new("reg.exe")
        .args(["export", &format!("{root}\\{}", k.path), &file.to_string_lossy(), "/y", arch])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if st.success() { Ok(()) } else { Err(io::Error::other("reg export не удался")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_in_hkcu() {
        let base = format!("Software\\StrelaTest-{}", std::process::id());
        let k = KeyPath::new(Hive::User, View::Native, base.clone());
        let key = RegKey::create(Hive::User, View::Native, &format!("{base}\\child")).unwrap();
        let s: Vec<u8> = "привет %TEMP%\0".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        key.set_raw("txt", REG_EXPAND_SZ, &s).unwrap();
        key.set_raw("num", REG_DWORD, &7u32.to_le_bytes()).unwrap();
        let v = key.values();
        assert_eq!(v.len(), 2);
        assert_eq!(key.get_dword("num"), Some(7));
        assert!(key.get_string("txt").unwrap().starts_with("привет "));
        assert!(!key.get_string("txt").unwrap().contains('%'));
        let top = k.open(false).unwrap();
        assert_eq!(top.subkeys(), vec!["child".to_string()]);
        let f = std::env::temp_dir().join(format!("strela-regtest-{}.reg", std::process::id()));
        export(&k, &f).unwrap();
        assert!(f.metadata().unwrap().len() > 20);
        let _ = std::fs::remove_file(&f);
        drop(key);
        drop(top);
        delete_tree(&k).unwrap();
        assert!(!k.exists());
        delete_tree(&k).unwrap(); // already gone
    }
}

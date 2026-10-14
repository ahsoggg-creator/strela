//! User settings, stored as JSON in `%LOCALAPPDATA%\Strela\settings.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use strela_core::transfer::HashAlgo;

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Default for Hotkey {
    fn default() -> Self {
        Hotkey { mods: MOD_CONTROL | MOD_ALT, vk: 0x20 } // Ctrl+Alt+Space
    }
}

impl Hotkey {
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mods & MOD_CONTROL != 0 {
            parts.push("Ctrl".into());
        }
        if self.mods & MOD_ALT != 0 {
            parts.push("Alt".into());
        }
        if self.mods & MOD_SHIFT != 0 {
            parts.push("Shift".into());
        }
        if self.mods & MOD_WIN != 0 {
            parts.push("Win".into());
        }
        parts.push(vk_name(self.vk));
        parts.join("+")
    }
}

pub fn vk_name(vk: u32) -> String {
    match vk {
        0x20 => "Пробел".into(),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0xC0 => "`".into(),
        0x0D => "Enter".into(),
        0x09 => "Tab".into(),
        v => format!("0x{v:X}"),
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub accent: [u8; 3],
    pub hotkey: Hotkey,
    pub autostart: bool,
    pub verify_copies: bool,
    pub hash: HashAlgo,
    /// Command to open a file at a line; `{path}` and `{line}` are replaced. Empty = system default.
    pub editor: String,
    /// Folders to index when no service is available; empty = all local drives.
    pub index_roots: Vec<String>,
    pub use_service: bool,
    pub fuzzy: bool,
    pub regex: bool,
    pub close_to_tray: bool,
    pub last_dest: String,
    /// Look for new releases on GitHub (one request to api.github.com about once a day).
    pub check_updates: bool,
    pub last_update_check: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemeChoice::System,
            accent: [45, 160, 235],
            hotkey: Hotkey::default(),
            autostart: true,
            verify_copies: false,
            hash: HashAlgo::Blake3,
            editor: String::new(),
            index_roots: Vec::new(),
            use_service: true,
            fuzzy: true,
            regex: false,
            close_to_tray: true,
            last_dest: String::new(),
            check_updates: true,
            last_update_check: 0,
        }
    }
}

pub fn dir() -> PathBuf {
    strela_core::util::data_dir()
}

fn file() -> PathBuf {
    dir().join("settings.json")
}

impl Settings {
    pub fn load() -> Settings {
        std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(dir());
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = file().with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, file());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_survive_partial_and_broken_files() {
        let s: Settings = serde_json::from_str(r#"{"theme":"Dark"}"#).unwrap();
        assert_eq!(s.theme, ThemeChoice::Dark);
        assert!(s.autostart && s.close_to_tray);
        assert_eq!(s.hotkey.describe(), "Ctrl+Alt+Пробел");
        assert!(serde_json::from_str::<Settings>("{not json").is_err());
    }

    #[test]
    fn hotkey_names() {
        assert_eq!(Hotkey { mods: MOD_CONTROL | MOD_SHIFT, vk: 0x46 }.describe(), "Ctrl+Shift+F");
        assert_eq!(Hotkey { mods: MOD_WIN, vk: 0x70 }.describe(), "Win+F1");
    }
}

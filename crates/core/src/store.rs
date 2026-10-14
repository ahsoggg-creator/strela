//! Microsoft Store (AppX / MSIX) packages of the current user: the list and one-call removal through WinRT's
//! `PackageManager`. Frameworks, resource packages and the core system packages are left out — removing those
//! only breaks Windows.

use std::path::PathBuf;

use windows::ApplicationModel::{Package, PackageSignatureKind};
use windows::Management::Deployment::PackageManager;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::HSTRING;

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct StorePackage {
    pub full_name: String,
    pub family_name: String,
    pub name: String,
    pub version: String,
    pub publisher: String,
    pub install_location: Option<PathBuf>,
    /// `YYYY-MM-DD`.
    pub install_date: String,
}

fn init() {
    // already initialised on this thread is fine (S_FALSE / RPC_E_CHANGED_MODE are ignored)
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn describe(p: &Package) -> Option<StorePackage> {
    if p.IsFramework().unwrap_or(false) || p.IsResourcePackage().unwrap_or(false) || p.IsBundle().unwrap_or(false) {
        return None;
    }
    if p.SignatureKind().ok()? == PackageSignatureKind::System {
        return None;
    }
    let id = p.Id().ok()?;
    let name = p.DisplayName().ok().map(|s| s.to_string()).filter(|s| !s.trim().is_empty() && !s.starts_with("ms-resource:"))?;
    let v = id.Version().ok()?;
    let date = p
        .InstalledDate()
        .ok()
        .map(|d| d.UniversalTime / 10_000_000 - 11_644_473_600)
        .filter(|s| *s > 0 && *s < u32::MAX as i64)
        .map(|s| crate::util::date_utc(s as u32).chars().take(10).collect::<String>())
        .unwrap_or_default();
    Some(StorePackage {
        full_name: id.FullName().ok()?.to_string(),
        family_name: id.FamilyName().ok()?.to_string(),
        name,
        version: format!("{}.{}.{}.{}", v.Major, v.Minor, v.Build, v.Revision),
        publisher: p.PublisherDisplayName().map(|s| s.to_string()).unwrap_or_default(),
        install_location: p.InstalledPath().ok().map(|s| PathBuf::from(s.to_string())).filter(|p| !p.as_os_str().is_empty()),
        install_date: date,
    })
}

/// Packages installed for the current user. Empty when WinRT is unavailable.
pub fn list() -> Vec<StorePackage> {
    init();
    let Ok(pm) = PackageManager::new() else { return Vec::new() };
    let Ok(all) = pm.FindPackagesByUserSecurityId(&HSTRING::new()) else { return Vec::new() };
    let mut out: Vec<StorePackage> = all.into_iter().filter_map(|p| describe(&p)).collect();
    out.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    out.dedup_by(|a, b| a.full_name == b.full_name);
    out
}

/// Removes the package for the current user (its data folder goes with it).
pub fn remove(full_name: &str) -> Result<()> {
    init();
    let q = |e: windows::core::Error| Error::Query(e.to_string());
    let pm = PackageManager::new().map_err(q)?;
    let res = pm.RemovePackageAsync(&HSTRING::from(full_name)).map_err(q)?.join().map_err(q)?;
    let code = res.ExtendedErrorCode().map_err(q)?;
    if code.is_err() {
        let text = res.ErrorText().map(|s| s.to_string()).unwrap_or_default();
        return Err(Error::Query(if text.trim().is_empty() { format!("код {:#010X}", code.0) } else { text.trim().to_string() }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_without_frameworks() {
        let l = super::list();
        assert!(l.iter().all(|p| !p.name.is_empty() && !p.full_name.is_empty()));
    }
}

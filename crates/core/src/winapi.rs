//! Thin WinAPI wrappers. All `unsafe` of the crate lives here.

use std::ffi::{OsString, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileAllocationInfo, FileDispositionInfoEx, FileFullDirectoryInfo,
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLongPathNameW, GetFileInformationByHandleEx, GetLogicalDriveStringsW,
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, MOVEFILE_REPLACE_EXISTING,
    MoveFileExW, OPEN_EXISTING, SetFileAttributesW, SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_NTFS_VOLUME_DATA,
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, IOCTL_STORAGE_QUERY_PROPERTY, NTFS_VOLUME_DATA_BUFFER,
    PropertyStandardQuery, STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
pub const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
pub const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const DELETE_ACCESS: u32 = 0x0001_0000;
const ERROR_NOT_SAME_DEVICE: i32 = 17;

pub fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide_nul(buf: &[u16]) -> OsString {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..n])
}

/// Adds the `\\?\` prefix so paths longer than MAX_PATH work. Relative paths are made absolute first.
pub fn long_path(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") {
        return p.to_path_buf();
    }
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    };
    // The `\?\` prefix switches off Win32 path normalisation, so `.` and `..` must be resolved here.
    let mut clean = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    let s = clean.to_string_lossy().replace('/', "\\");
    if let Some(rest) = s.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{rest}"))
    } else {
        PathBuf::from(format!(r"\\?\{s}"))
    }
}

/// Strips the `\\?\` prefix again for display.
pub fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{r}")
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        r.to_string()
    } else {
        s.into_owned()
    }
}

pub struct OwnedH(pub HANDLE);

impl Drop for OwnedH {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful CreateFileW/OpenProcessToken and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: out-pointer references a live local; the pseudo handle needs no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = OwnedH(token);
    let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut ret = 0u32;
    // SAFETY: the buffer is a live TOKEN_ELEVATION of the stated size.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            &mut elev as *mut TOKEN_ELEVATION as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    };
    ok != 0 && elev.TokenIsElevated != 0
}

/// Mount root of the volume containing `path` (walks up to an existing ancestor first).
pub fn volume_root(path: &Path) -> io::Result<PathBuf> {
    let mut p = long_path(path);
    while !p.exists() {
        match p.parent() {
            Some(par) if par.as_os_str().len() > 4 => p = par.to_path_buf(),
            _ => break,
        }
    }
    let w = wide(&p);
    let mut buf = vec![0u16; 1024];
    // SAFETY: `w` is NUL-terminated; `buf` has the stated capacity.
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(from_wide_nul(&buf)))
}

/// Stable identity of a volume: its `\\?\Volume{GUID}\` name (works for mounted folders too).
pub fn volume_guid(path: &Path) -> io::Result<String> {
    let root = volume_root(path)?;
    let mut r = root.as_os_str().to_string_lossy().into_owned();
    if !r.ends_with('\\') {
        r.push('\\');
    }
    let w = wide_str(&r);
    let mut buf = vec![0u16; 128];
    // SAFETY: `w` is NUL-terminated; `buf` has the stated capacity.
    let ok = unsafe { GetVolumeNameForVolumeMountPointW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if ok == 0 {
        // Network shares and the like have no GUID name: fall back to the root path itself.
        return Ok(display_path(&root).to_lowercase());
    }
    Ok(from_wide_nul(&buf).to_string_lossy().to_lowercase())
}

pub fn same_volume(a: &Path, b: &Path) -> bool {
    match (volume_guid(a), volume_guid(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub fs_name: String,
    pub flags: u32,
    pub serial: u32,
}

impl VolumeInfo {
    pub fn supports_block_cloning(&self) -> bool {
        self.flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0
    }
    pub fn is_ntfs(&self) -> bool {
        self.fs_name.eq_ignore_ascii_case("NTFS")
    }
}

pub fn volume_info(path: &Path) -> io::Result<VolumeInfo> {
    let root = volume_root(path)?;
    let mut r = root.as_os_str().to_string_lossy().into_owned();
    if !r.ends_with('\\') {
        r.push('\\');
    }
    let w = wide_str(&r);
    let (mut serial, mut maxc, mut flags) = (0u32, 0u32, 0u32);
    let mut fs = vec![0u16; 64];
    // SAFETY: `w` is NUL-terminated; out-pointers reference live locals; `fs` has the stated capacity.
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            &mut maxc,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(VolumeInfo {
        fs_name: from_wide_nul(&fs).to_string_lossy().into_owned(),
        flags,
        serial,
    })
}

/// `(total, free)` bytes of the volume containing `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let w = wide(path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `w` is NUL-terminated; out-pointers reference live locals.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0 && total > 0).then_some((total, avail))
}

/// Puts the calling thread into background mode: lower CPU priority and, more importantly, low I/O priority.
pub fn lower_current_thread_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
    const THREAD_MODE_BACKGROUND_BEGIN: i32 = 0x0001_0000;
    // SAFETY: operates on the calling thread's pseudo handle.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
}

/// Unix seconds -> local `(year, month, day, hour, minute)` using the machine's time zone rules for that date.
pub fn unix_to_local(unix: u32) -> Option<(u16, u16, u16, u16, u16)> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let t = (unix as u64 + 11_644_473_600) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    // SAFETY: SYSTEMTIME is plain-old-data; all pointers reference live locals.
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return None;
        }
        Some((local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute))
    }
}

/// Drive letters of local fixed volumes (and removable ones when `removable`).
pub fn drive_letters(removable: bool) -> Vec<char> {
    let mut buf = vec![0u16; 512];
    // SAFETY: `buf` has the stated capacity.
    let n = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
    let mut out = Vec::new();
    for root in buf[..n.min(buf.len())].split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let mut r = root.to_vec();
        r.push(0);
        // SAFETY: `r` is NUL-terminated.
        let t = unsafe { GetDriveTypeW(r.as_ptr()) };
        let ok = t == 3 || (removable && t == 2);
        if ok && let Some(c) = char::from_u32(root[0] as u32) {
            out.push(c.to_ascii_uppercase());
        }
    }
    out
}

/// Whether the drive holding `path` is a spinning disk (`Some(true)`), an SSD (`Some(false)`) or unknown.
pub fn incurs_seek_penalty(path: &Path) -> Option<bool> {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let mut it = s.chars();
    let letter = it.next().filter(char::is_ascii_alphabetic)?;
    if it.next() != Some(':') {
        return None;
    }
    let vol = wide_str(&format!(r"\\.\{}:", letter.to_ascii_uppercase()));
    // SAFETY: `vol` is NUL-terminated; no security attributes or template handle are passed.
    let h = unsafe {
        CreateFileW(
            vol.as_ptr(),
            0,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = OwnedH(h);
    // SAFETY: both structures are plain-old-data; all-zero is valid (PropertyStandardQuery == 0).
    let mut query: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
    query.PropertyId = StorageDeviceSeekPenaltyProperty;
    query.QueryType = PropertyStandardQuery;
    let mut out: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: pointers/sizes describe live locals; the handle is valid and synchronous.
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const STORAGE_PROPERTY_QUERY as *const c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut out as *mut DEVICE_SEEK_PENALTY_DESCRIPTOR as *mut c_void,
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    (ok != 0 && returned as usize >= std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>())
        .then_some(out.IncursSeekPenalty)
}

// ---------------------------------------------------------------------------------------------------------
// Moves, attributes, allocation

/// Renames/moves within one volume. Without `MOVEFILE_COPY_ALLOWED` this fails across volumes instead of
/// silently copying, which is exactly the O(1) guarantee the same-volume path wants.
pub fn move_file_ex(src: &Path, dst: &Path, replace: bool) -> io::Result<()> {
    let (s, d) = (wide(&long_path(src)), wide(&long_path(dst)));
    let flags = if replace { MOVEFILE_REPLACE_EXISTING } else { 0 };
    // SAFETY: both paths are NUL-terminated.
    let ok = unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), flags) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

pub fn is_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE)
}

pub fn set_attributes(path: &Path, attrs: u32) -> io::Result<()> {
    let w = wide(&long_path(path));
    let a = if attrs == 0 { FILE_ATTRIBUTE_NORMAL } else { attrs };
    // SAFETY: `w` is NUL-terminated.
    let ok = unsafe { SetFileAttributesW(w.as_ptr(), a) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Reserves `size` bytes of disk space for the file without changing its length (fewer fragments, early
/// "disk full" instead of a failure halfway through).
pub fn preallocate(file: &File, size: u64) -> io::Result<()> {
    let info = FILE_ALLOCATION_INFO {
        AllocationSize: size as i64,
    };
    // SAFETY: the handle is valid for the lifetime of `file`; the buffer is a live FILE_ALLOCATION_INFO.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileAllocationInfo,
            &info as *const FILE_ALLOCATION_INFO as *const c_void,
            std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// ReFS / Dev Drive block cloning: makes `dst` share the clusters of `src` (copy-on-write). `dst` must be on
/// the same volume. Returns an error (and the caller falls back to a normal copy) if the file system
/// does not support it.
pub fn clone_file(src: &File, dst: &File, len: u64, cluster: u64) -> io::Result<()> {
    #[repr(C)]
    struct DuplicateExtentsData {
        file_handle: HANDLE,
        source_offset: i64,
        target_offset: i64,
        byte_count: i64,
    }
    if len == 0 {
        return Ok(());
    }
    dst.set_len(len)?;
    // Each call is limited; 1 GiB (cluster aligned) pieces are well within every documented limit.
    let piece = (1u64 << 30) / cluster * cluster;
    let mut off = 0u64;
    while off < len {
        let want = (len - off).min(piece);
        // The last piece may end inside a cluster; the API wants whole clusters, so round the count up.
        let count = want.div_ceil(cluster) * cluster;
        let data = DuplicateExtentsData {
            file_handle: src.as_raw_handle(),
            source_offset: off as i64,
            target_offset: off as i64,
            byte_count: count as i64,
        };
        let mut ret = 0u32;
        // SAFETY: `data` is a live DUPLICATE_EXTENTS_DATA; both handles are valid for the call.
        let ok = unsafe {
            DeviceIoControl(
                dst.as_raw_handle(),
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                &data as *const DuplicateExtentsData as *const c_void,
                std::mem::size_of::<DuplicateExtentsData>() as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        off += want;
    }
    Ok(())
}

pub fn cluster_size(path: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
    let root = volume_root(path).ok()?;
    let mut r = root.as_os_str().to_string_lossy().into_owned();
    if !r.ends_with('\\') {
        r.push('\\');
    }
    let w = wide_str(&r);
    let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: `w` is NUL-terminated; out-pointers reference live locals.
    let ok = unsafe { GetDiskFreeSpaceW(w.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) };
    (ok != 0 && spc > 0 && bps > 0).then_some(spc as u64 * bps as u64)
}

// ---------------------------------------------------------------------------------------------------------
// Deletion

/// Deletes one file or empty directory with POSIX semantics: the name disappears immediately even if
/// another process still holds the file open, and read-only files do not need an attribute reset.
/// Reparse points (symlinks, junctions) are deleted themselves, never followed.
pub fn delete_posix(path: &Path) -> io::Result<()> {
    let p = long_path(path);
    let file = OpenOptions::new()
        .access_mode(DELETE_ACCESS | FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&p)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: the handle is valid for the lifetime of `file`; the buffer is a live FILE_DISPOSITION_INFO_EX.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            &info as *const FILE_DISPOSITION_INFO_EX as *const c_void,
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

// ---------------------------------------------------------------------------------------------------------
// Directory listing

/// One entry of a directory listing; `name` is UTF-16 without terminator.
pub struct RawEntry<'a> {
    pub name: &'a [u16],
    pub attributes: u32,
    pub size: u64,
    pub alloc: u64,
    /// FILETIME (100 ns ticks since 1601).
    pub mtime: i64,
}

impl RawEntry<'_> {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
    pub fn is_reparse(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

const LIST_BUF_WORDS: usize = 8192; // 64 KiB, 8-byte aligned

thread_local! {
    static LIST_BUF: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(vec![0u64; LIST_BUF_WORDS]);
}

/// Lists a directory with `GetFileInformationByHandleEx(FileFullDirectoryInfo)`: one syscall returns as many
/// entries as fit into 64 KiB, with sizes and timestamps — no per-file calls. `.` and `..` are skipped.
pub fn list_dir(path: &Path, mut f: impl FnMut(RawEntry)) -> io::Result<()> {
    let w = wide(&long_path(path));
    // SAFETY: `w` is NUL-terminated; no security attributes or template handle are passed.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            FILE_LIST_DIRECTORY,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let h = OwnedH(h);
    LIST_BUF.with_borrow_mut(|buf| {
        let cap = buf.len() * 8;
        loop {
            // SAFETY: `buf` is a writable, 8-byte aligned buffer of `cap` bytes; the handle is a valid directory.
            let ok = unsafe {
                GetFileInformationByHandleEx(h.0, FileFullDirectoryInfo, buf.as_mut_ptr() as *mut c_void, cap as u32)
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) { Ok(()) } else { Err(e) };
            }
            // SAFETY: the buffer is initialised memory of `cap` bytes; u8 has no alignment requirement.
            let b: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, cap) };
            parse_full_dir_info(b, &mut f);
        }
    })
}

/// Walks the FILE_FULL_DIR_INFO chain in `b`.
fn parse_full_dir_info(b: &[u8], f: &mut impl FnMut(RawEntry)) {
    const NAME: usize = 0x44;
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let i64_at = |o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let mut name_buf: Vec<u16> = Vec::new();
    let mut off = 0usize;
    loop {
        if off + NAME > b.len() {
            return;
        }
        let next = u32_at(off) as usize;
        let name_bytes = u32_at(off + 0x3C) as usize;
        let start = off + NAME;
        if start + name_bytes > b.len() {
            return;
        }
        name_buf.clear();
        name_buf.extend(b[start..start + name_bytes].as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p)));
        let dot = u16::from(b'.');
        let is_dot = matches!(name_buf.as_slice(), [d] | [d, _] if *d == dot) && name_buf.iter().all(|&c| c == dot);
        if !is_dot {
            f(RawEntry {
                name: &name_buf,
                attributes: u32_at(off + 0x38),
                size: i64_at(off + 0x28).max(0) as u64,
                alloc: i64_at(off + 0x30).max(0) as u64,
                mtime: i64_at(off + 0x18),
            });
        }
        if next == 0 {
            return;
        }
        off += next;
    }
}

// ---------------------------------------------------------------------------------------------------------
// Raw volume / USN journal

#[repr(C)]
struct VolumeDataOut {
    base: NTFS_VOLUME_DATA_BUFFER,
    extended: [u8; 64],
}

pub fn ntfs_volume_data(volume: &File) -> io::Result<NTFS_VOLUME_DATA_BUFFER> {
    // SAFETY: plain-old-data, all-zero is a valid value.
    let mut out: VolumeDataOut = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: handle valid; the output pointer/size describe `out`; synchronous handle, no OVERLAPPED.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            &mut out as *mut VolumeDataOut as *mut c_void,
            std::mem::size_of::<VolumeDataOut>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(out.base)
}

/// Opens `\\.\X:` read-only with full sharing (needs administrator rights).
pub fn open_volume(drive: char) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(format!(r"\\.\{drive}:"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnJournal {
    pub id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid: i64,
}

pub fn query_usn_journal(volume: &File) -> io::Result<UsnJournal> {
    let mut out = [0u8; 80];
    let mut returned = 0u32;
    // SAFETY: handle valid; the buffer is a live byte array of the stated size.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || returned < 56 {
        return Err(io::Error::last_os_error());
    }
    let u64_at = |o: usize| u64::from_le_bytes(out[o..o + 8].try_into().unwrap());
    Ok(UsnJournal {
        id: u64_at(0),
        first_usn: u64_at(8) as i64,
        next_usn: u64_at(16) as i64,
        lowest_valid: u64_at(24) as i64,
    })
}

/// Reads USN records starting at `start`. The returned buffer begins with the next USN (8 bytes) followed by
/// `USN_RECORD_V2` entries. `wait_secs > 0` blocks until data arrives or the timeout elapses.
pub fn read_usn_journal(volume: &File, journal_id: u64, start: i64, wait_secs: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut input = [0u8; 40];
    input[0..8].copy_from_slice(&start.to_le_bytes());
    input[8..12].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // ReasonMask: everything
    input[12..16].copy_from_slice(&0u32.to_le_bytes()); // ReturnOnlyOnClose = FALSE
    input[16..24].copy_from_slice(&wait_secs.to_le_bytes()); // Timeout
    let wait_bytes: u64 = if wait_secs > 0 { 1 } else { 0 };
    input[24..32].copy_from_slice(&wait_bytes.to_le_bytes()); // BytesToWaitFor
    input[32..40].copy_from_slice(&journal_id.to_le_bytes());
    let mut returned = 0u32;
    // SAFETY: handle valid; input/output pointers describe live buffers of the stated sizes.
    let ok = unsafe {
        DeviceIoControl(
            volume.as_raw_handle(),
            FSCTL_READ_USN_JOURNAL,
            input.as_ptr() as *const c_void,
            input.len() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(returned as usize) }
}

/// Recursive directory change watcher built on overlapped `ReadDirectoryChangesW`, so a stop flag can interrupt
/// the wait (`CancelIoEx`) instead of blocking the thread until the next change.
pub struct DirWatcher {
    file: File,
    event: OwnedH,
    buf: Vec<u64>,
}

// SAFETY: the raw event handle is only used from the thread that owns the watcher; moving the whole watcher to
// another thread is fine.
unsafe impl Send for DirWatcher {}

impl DirWatcher {
    pub fn open(path: &Path) -> io::Result<DirWatcher> {
        use windows_sys::Win32::System::Threading::CreateEventW;
        const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
        let w = wide(&long_path(path));
        // SAFETY: `w` is NUL-terminated.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_LIST_DIRECTORY,
                SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `h` is a fresh valid handle whose ownership moves into the File.
        let file = unsafe { File::from_raw_handle(h) };
        // SAFETY: manual-reset, initially non-signalled, unnamed event.
        let ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if ev.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(DirWatcher { file, event: OwnedH(ev), buf: vec![0u64; 16384] })
    }

    /// Waits for the next batch of changes. `Ok(None)`: stopped. `Ok(Some(bytes))`: raw FILE_NOTIFY_INFORMATION
    /// records; an empty vector means the kernel buffer overflowed and changes were lost.
    pub fn next(&mut self, stop: &std::sync::atomic::AtomicBool) -> io::Result<Option<Vec<u8>>> {
        use std::sync::atomic::Ordering::Relaxed;
        use windows_sys::Win32::Storage::FileSystem::ReadDirectoryChangesW;
        use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
        use windows_sys::Win32::System::Threading::{ResetEvent, WaitForSingleObject};
        const FILTER: u32 = 0x1 | 0x2 | 0x8 | 0x10 | 0x40; // names, size, last write, creation
        const WAIT_OBJECT_0: u32 = 0;
        // SAFETY: OVERLAPPED is plain-old-data; all-zero is valid.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event.0;
        // SAFETY: the event handle is valid.
        unsafe { ResetEvent(self.event.0) };
        let cap = (self.buf.len() * 8) as u32;
        // SAFETY: handle valid; buffer is 8-byte aligned with the stated size and, like `ov`, outlives the request
        // because we always wait for completion (or cancel and wait) before returning.
        let ok = unsafe {
            ReadDirectoryChangesW(
                self.file.as_raw_handle(),
                self.buf.as_mut_ptr() as *mut c_void,
                cap,
                1,
                FILTER,
                std::ptr::null_mut(),
                &mut ov,
                None,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        loop {
            // SAFETY: the event handle is valid.
            let r = unsafe { WaitForSingleObject(self.event.0, 300) };
            if r == WAIT_OBJECT_0 {
                break;
            }
            if stop.load(Relaxed) {
                // SAFETY: cancels our single pending request on this handle; then wait for it to finish so the
                // buffer is no longer referenced by the kernel.
                unsafe { CancelIoEx(self.file.as_raw_handle(), &ov) };
                let mut n = 0u32;
                // SAFETY: waiting (bWait = 1) for the cancelled request.
                unsafe { GetOverlappedResult(self.file.as_raw_handle(), &ov, &mut n, 1) };
                return Ok(None);
            }
        }
        let mut n = 0u32;
        // SAFETY: the request has completed (event signalled).
        let ok = unsafe { GetOverlappedResult(self.file.as_raw_handle(), &ov, &mut n, 0) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes: &[u8] = {
            // SAFETY: `buf` holds at least `n` initialised bytes.
            unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const u8, n as usize) }
        };
        Ok(Some(bytes.to_vec()))
    }
}

/// Expands 8.3 short names (`C:\PROGRA~1`) of an existing path; `None` if the path does not exist.
pub fn long_name(path: &Path) -> Option<PathBuf> {
    let w = wide(path);
    let mut buf = vec![0u16; 1024];
    // SAFETY: `w` is NUL-terminated; `buf` has the stated capacity.
    let n = unsafe { GetLongPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n > buf.len() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..n])))
}

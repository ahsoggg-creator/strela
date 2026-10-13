//! The always-on part of Strela: a hidden message window on its own thread that owns the tray icon (with a live
//! progress ring), the global hotkey and the balloon notifications. It keeps working while the main window is
//! hidden, and it shows the window with plain Win32 calls, so nothing depends on the GUI loop being awake.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, OnceLock};

use strela_core::transfer::queue::JobState;
use strela_core::util;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BringWindowToTop, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DispatchMessageW, EnumWindows, GetCursorPos, GetForegroundWindow, GetMessageW, GetSystemMetrics,
    GetWindowTextW, GetWindowThreadProcessId, HWND_MESSAGE, ICONINFO, IsIconic, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    MSG, PostMessageW, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow,
    SetTimer, ShowWindow, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CONTEXTMENU,
    WM_DESTROY, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
    CreateIconIndirect,
};

use crate::commands::{self, Command};
use crate::state::{Shared, Tab, UiEvent};

const WM_TRAY: u32 = WM_APP + 1;
const WM_WAKE: u32 = WM_APP + 2;
const WM_REHOTKEY: u32 = WM_APP + 3;
const HOTKEY_ID: i32 = 1;
const TIMER_ID: usize = 1;
const MOD_NOREPEAT: u32 = 0x4000;
const CMD_OPEN: usize = 1;
const CMD_SEARCH: usize = 2;
const CMD_TRANSFER: usize = 3;
const CMD_AUTOSTART: usize = 4;
const CMD_QUIT: usize = 5;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
const BASE_ICON: &[u8] = include_bytes!("../assets/icon32.rgba");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hwnd_of(v: isize) -> HWND {
    v as HWND
}

pub fn start(shared: Arc<Shared>) {
    let _ = SHARED.set(shared.clone());
    std::thread::Builder::new()
        .name("strela-shell".into())
        .spawn(move || run(shared))
        .expect("spawn shell thread");
}

fn run(shared: Arc<Shared>) {
    // SAFETY: standard window-class registration and message loop on this thread.
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide("StrelaShellWindow");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Strela").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        shared.shell_hwnd.store(hwnd as isize, Relaxed);
        add_tray(hwnd, None, "Strela");
        register_hotkey(&shared);
        SetTimer(hwnd, TIMER_ID, 400, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn register_hotkey(shared: &Shared) {
    let hwnd = hwnd_of(shared.shell_hwnd.load(Relaxed));
    let hk = shared.settings.read().unwrap().hotkey;
    // SAFETY: plain Win32 calls with our own window handle.
    unsafe {
        UnregisterHotKey(hwnd, HOTKEY_ID);
        let ok = RegisterHotKey(hwnd, HOTKEY_ID, hk.mods | MOD_NOREPEAT, hk.vk);
        *shared.hotkey_error.lock().unwrap() = if ok == 0 {
            Some(format!(
                "Сочетание {} уже занято другой программой. Выберите другое в настройках.",
                hk.describe()
            ))
        } else {
            None
        };
    }
}

/// Re-reads the hotkey from the settings (called after the user changed it).
pub fn reregister_hotkey(sh: &Shared) {
    let h = sh.shell_hwnd.load(Relaxed);
    if h != 0 {
        // SAFETY: posting to our own window.
        unsafe { PostMessageW(hwnd_of(h), WM_REHOTKEY, 0, 0) };
    }
}

pub fn wake(sh: &Shared) {
    let h = sh.shell_hwnd.load(Relaxed);
    if h != 0 {
        // SAFETY: posting to our own window.
        unsafe { PostMessageW(hwnd_of(h), WM_WAKE, 0, 0) };
    }
}

pub fn remove_tray(sh: &Shared) {
    let h = sh.shell_hwnd.load(Relaxed);
    if h != 0 {
        // SAFETY: NIM_DELETE identifies the icon by window + id only.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd_of(h);
            nid.uID = 1;
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

fn copy_wide<const N: usize>(dst: &mut [u16; N], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(N - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

fn add_tray(hwnd: HWND, progress: Option<f32>, tip: &str) {
    // SAFETY: NOTIFYICONDATAW is plain-old-data; the icon handle is owned by the tray until replaced.
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = make_icon(progress);
        copy_wide(&mut nid.szTip, tip);
        Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn modify_tray(hwnd: HWND, progress: Option<f32>, tip: &str) {
    // SAFETY: as in `add_tray`; the previous icon is destroyed after the shell has switched to the new one.
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = make_icon(progress);
        copy_wide(&mut nid.szTip, tip);
        Shell_NotifyIconW(NIM_MODIFY, &nid);
        DestroyIcon(nid.hIcon);
    }
}

fn balloon(hwnd: HWND, title: &str, text: &str, error: bool) {
    // SAFETY: as above.
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_INFO;
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, text);
        nid.dwInfoFlags = if error { 3 } else { 1 }; // NIIF_ERROR / NIIF_INFO
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// App icon scaled to the tray size, with a progress ring when `progress` is given.
fn render_icon(size: usize, progress: Option<f32>, accent: [u8; 3]) -> Vec<u8> {
    let sample = |u: f32, v: f32| -> [f32; 4] {
        // bilinear sample of the 32x32 base icon, u/v in 0..1
        let (x, y) = (u * 31.0, v * 31.0);
        let (x0, y0) = (x.floor().clamp(0.0, 31.0) as usize, y.floor().clamp(0.0, 31.0) as usize);
        let (x1, y1) = ((x0 + 1).min(31), (y0 + 1).min(31));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let px = |xx: usize, yy: usize| -> [f32; 4] {
            let i = (yy * 32 + xx) * 4;
            [BASE_ICON[i] as f32, BASE_ICON[i + 1] as f32, BASE_ICON[i + 2] as f32, BASE_ICON[i + 3] as f32]
        };
        let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
        let mut out = [0.0; 4];
        for k in 0..4 {
            out[k] = (a[k] * (1.0 - fx) + b[k] * fx) * (1.0 - fy) + (c[k] * (1.0 - fx) + d[k] * fx) * fy;
        }
        out
    };
    let mut bgra = vec![0u8; size * size * 4];
    let c = (size as f32 - 1.0) / 2.0;
    let r_out = size as f32 / 2.0;
    let r_in = r_out * 0.70;
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let d = (dx * dx + dy * dy).sqrt();
            let mut px = [0.0f32; 4]; // r g b a
            match progress {
                None => {
                    px = sample((x as f32 + 0.5) / size as f32, (y as f32 + 0.5) / size as f32);
                }
                Some(p) => {
                    if d <= r_out && d >= r_in {
                        let mut ang = dx.atan2(-dy);
                        if ang < 0.0 {
                            ang += std::f32::consts::TAU;
                        }
                        let on = ang / std::f32::consts::TAU <= p;
                        let edge = ((r_out - d).min(d - r_in) + 0.5).clamp(0.0, 1.0);
                        px = if on {
                            [accent[0] as f32, accent[1] as f32, accent[2] as f32, 255.0 * edge]
                        } else {
                            [160.0, 160.0, 160.0, 110.0 * edge]
                        };
                    } else if d < r_in {
                        // the app icon, shrunk into the ring
                        let s = (r_in - 0.5) * std::f32::consts::SQRT_2;
                        let (u, v) = (0.5 + dx / (2.0 * s), 0.5 + dy / (2.0 * s));
                        if (0.0..1.0).contains(&u) && (0.0..1.0).contains(&v) {
                            px = sample(u, v);
                        }
                    }
                }
            }
            let i = (y * size + x) * 4;
            bgra[i] = px[2] as u8;
            bgra[i + 1] = px[1] as u8;
            bgra[i + 2] = px[0] as u8;
            bgra[i + 3] = px[3] as u8;
        }
    }
    bgra
}

fn make_icon(progress: Option<f32>) -> *mut c_void {
    // SAFETY: GDI object creation; the bitmaps are deleted right after CreateIconIndirect copied them.
    unsafe {
        let size = (GetSystemMetrics(SM_CXSMICON).max(16)) as usize;
        let accent = SHARED.get().map_or([45, 160, 235], |s| s.settings.read().unwrap().accent);
        let bgra = render_icon(size, progress, accent);
        let color = CreateBitmap(size as i32, size as i32, 1, 32, bgra.as_ptr() as *const c_void);
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, std::ptr::null());
        let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

/// Aggregate of the queue for the tray: `(progress 0..1, tooltip)`.
fn queue_summary(sh: &Shared, prev: &mut HashMap<u64, JobState>) -> (Option<f32>, String) {
    let jobs = sh.queue.list();
    let (mut done, mut total, mut speed, mut active) = (0u64, 0u64, 0f64, 0usize);
    for j in &jobs {
        if matches!(j.state, JobState::Running | JobState::Paused | JobState::Queued) {
            active += 1;
            done += j.progress.bytes_done;
            total += j.progress.bytes_total;
            speed += j.progress.speed;
        }
        let was = prev.insert(j.id, j.state);
        if was != Some(j.state) && was.is_some_and(|w| !w.is_finished()) {
            match j.state {
                JobState::Done => {
                    if let Some(r) = &j.report {
                        let what = if r.instant_moves > 0 && r.bytes == 0 {
                            format!("Перемещено мгновенно: {} элем.", r.instant_moves)
                        } else {
                            format!("Готово: {} файлов, {} за {}", r.files_copied, util::bytes(r.bytes), util::clock(r.elapsed))
                        };
                        sh.balloons.lock().unwrap().push_back(("Strela".into(), what, false));
                    }
                }
                JobState::DoneWithErrors => {
                    let n = j.report.as_ref().map_or(0, |r| r.errors.len());
                    sh.balloons.lock().unwrap().push_back(("Strela".into(), format!("Завершено с ошибками: {n}"), true));
                }
                JobState::Failed => {
                    let m = j.error.clone().unwrap_or_default();
                    sh.balloons.lock().unwrap().push_back(("Strela: ошибка".into(), m, true));
                }
                _ => {}
            }
        }
    }
    if active == 0 {
        return (None, "Strela".into());
    }
    let frac = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
    let tip = if total > 0 {
        format!("Strela — {:.0}% · {} · {} операций", frac * 100.0, util::speed(speed), active)
    } else {
        format!("Strela — {active} операций")
    };
    (Some(frac), tip)
}

fn show_tray_menu(hwnd: HWND, sh: &Arc<Shared>) {
    // SAFETY: menu creation and tracking on the owning thread; the menu is destroyed afterwards.
    unsafe {
        let menu = CreatePopupMenu();
        let hk = sh.settings.read().unwrap().hotkey.describe();
        let autostart = sh.settings.read().unwrap().autostart;
        AppendMenuW(menu, MF_STRING, CMD_OPEN, wide("Открыть Strela").as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_SEARCH, wide(&format!("Быстрый поиск\t{hk}")).as_ptr());
        AppendMenuW(menu, MF_STRING, CMD_TRANSFER, wide("Передача файлов").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(
            menu,
            MF_STRING | if autostart { MF_CHECKED } else { 0 },
            CMD_AUTOSTART,
            wide("Запускать при входе в Windows").as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, CMD_QUIT, wide("Выход").as_ptr());
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, std::ptr::null()) as usize;
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd {
            CMD_OPEN => {
                sh.push_event(UiEvent::Show(Tab::Search));
                show_main(sh);
            }
            CMD_SEARCH => commands::execute(sh, Command { verb: "spotlight".into(), paths: vec![], dest: None }),
            CMD_TRANSFER => {
                sh.push_event(UiEvent::Show(Tab::Transfer));
                show_main(sh);
            }
            CMD_AUTOSTART => {
                let on = {
                    let mut s = sh.settings.write().unwrap();
                    s.autostart = !s.autostart;
                    s.save();
                    s.autostart
                };
                let _ = crate::winint::set_autostart(on);
                sh.repaint();
            }
            CMD_QUIT => sh.quit(),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(sh) = SHARED.get() else {
        // SAFETY: default processing for messages before initialisation.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    thread_local! {
        static PREV: std::cell::RefCell<HashMap<u64, JobState>> = std::cell::RefCell::new(HashMap::new());
        static LAST: std::cell::RefCell<(Option<u32>, String)> = const { std::cell::RefCell::new((None, String::new())) };
    }
    match msg {
        WM_TRAY => {
            match (lparam as u32) & 0xFFFF {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                    sh.push_event(UiEvent::Show(Tab::Search));
                    show_main(sh);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => show_tray_menu(hwnd, sh),
                _ => {}
            }
            0
        }
        WM_HOTKEY => {
            commands::execute(sh, Command { verb: "spotlight".into(), paths: vec![], dest: None });
            0
        }
        WM_REHOTKEY => {
            register_hotkey(sh);
            0
        }
        WM_WAKE => {
            drain_balloons(hwnd, sh);
            0
        }
        WM_TIMER => {
            let (p, tip) = PREV.with_borrow_mut(|prev| queue_summary(sh, prev));
            let bucket = p.map(|f| (f * 40.0) as u32);
            let changed = LAST.with_borrow_mut(|l| {
                let c = l.0 != bucket || l.1 != tip;
                *l = (bucket, tip.clone());
                c
            });
            if changed {
                modify_tray(hwnd, p, &tip);
            }
            drain_balloons(hwnd, sh);
            0
        }
        WM_DESTROY => {
            remove_tray(sh);
            0
        }
        m if Some(&m) == TASKBAR_CREATED.get() => {
            add_tray(hwnd, None, "Strela");
            0
        }
        // SAFETY: default processing.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn drain_balloons(hwnd: HWND, sh: &Shared) {
    let items: Vec<_> = sh.balloons.lock().unwrap().drain(..).collect();
    for (title, text, error) in items {
        balloon(hwnd, &title, &text, error);
    }
}

// ---------------------------------------------------------------------------------------------------------
// Main window helpers

unsafe extern "system" fn find_cb(hwnd: HWND, lparam: LPARAM) -> i32 {
    // SAFETY: lparam points to a (pid, found) pair on the caller's stack; hwnd is valid during enumeration.
    unsafe {
        let out = &mut *(lparam as *mut (u32, isize));
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == out.0 {
            let mut buf = [0u16; 64];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if n > 0 && String::from_utf16_lossy(&buf[..n as usize]).starts_with("Strela") {
                out.1 = hwnd as isize;
                return 0;
            }
        }
    }
    1
}

pub fn find_main_window() -> isize {
    let mut out = (std::process::id(), 0isize);
    // SAFETY: the callback only touches `out`, which outlives the call.
    unsafe { EnumWindows(Some(find_cb), &mut out as *mut (u32, isize) as LPARAM) };
    out.1
}

fn main_hwnd(sh: &Shared) -> isize {
    let h = sh.main_hwnd.load(Relaxed);
    if h != 0 {
        return h;
    }
    let h = find_main_window();
    sh.main_hwnd.store(h, Relaxed);
    h
}

/// Brings the main window to the front even when Windows would normally refuse a background process.
pub fn force_foreground(hwnd: HWND) {
    // SAFETY: Win32 window calls on a window of this process.
    unsafe {
        let fg = GetForegroundWindow();
        let (fg_thread, our) = (GetWindowThreadProcessId(fg, std::ptr::null_mut()), GetCurrentThreadId());
        if fg_thread != 0 && fg_thread != our {
            AttachThreadInput(our, fg_thread, 1);
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
            AttachThreadInput(our, fg_thread, 0);
        } else {
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
        }
    }
}

pub fn show_main(sh: &Shared) {
    let h = main_hwnd(sh);
    if h == 0 {
        return;
    }
    let hwnd = hwnd_of(h);
    // SAFETY: window of this process.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
    }
    force_foreground(hwnd);
    sh.repaint();
}

pub fn hide_main(sh: &Shared) {
    let h = main_hwnd(sh);
    if h != 0 {
        // SAFETY: window of this process.
        unsafe { ShowWindow(hwnd_of(h), SW_HIDE) };
    }
}

#[cfg(test)]
mod tests {
    use super::render_icon;

    #[test]
    fn icon_ring_grows_with_progress() {
        let count_accent = |p: f32| {
            let px = render_icon(24, Some(p), [10, 200, 30]);
            px.as_chunks::<4>().0.iter().filter(|c| c[1] == 200 && c[0] == 30 && c[3] == 255).count()
        };
        assert_eq!(count_accent(0.0), 0);
        assert!(count_accent(0.5) > 5);
        assert!(count_accent(1.0) > count_accent(0.5));
        // no-progress icon is the plain app icon (opaque somewhere, transparent in the corners)
        let plain = render_icon(24, None, [0, 0, 0]);
        assert_eq!(plain[3], 0);
        assert!(plain.as_chunks::<4>().0.iter().any(|c| c[3] == 255));
    }
}

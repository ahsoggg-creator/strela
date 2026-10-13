//! System file icons (the same ones Explorer shows), fetched per extension and cached as egui textures.

use std::collections::HashMap;
use std::ffi::c_void;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use windows_sys::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits, GetObjectW,
};
use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

#[derive(Default)]
pub struct Icons {
    map: HashMap<String, Option<TextureHandle>>,
}

impl Icons {
    /// Icon for a file name (by extension) or a folder; `None` if the system gave nothing.
    pub fn get(&mut self, ctx: &egui::Context, name: &str, is_dir: bool) -> Option<TextureHandle> {
        let key = if is_dir {
            "<dir>".to_string()
        } else {
            name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
        };
        if let Some(t) = self.map.get(&key) {
            return t.clone();
        }
        let probe = if is_dir { "folder".to_string() } else { format!("file.{key}") };
        let tex = load(&probe, is_dir).map(|img| ctx.load_texture(format!("icon-{key}"), img, TextureOptions::LINEAR));
        self.map.insert(key, tex.clone());
        tex
    }
}

fn load(name: &str, is_dir: bool) -> Option<ColorImage> {
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: SHFILEINFOW is plain-old-data; the name is NUL-terminated; USEFILEATTRIBUTES means no disk access.
    unsafe {
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
        let r = SHGetFileInfoW(w.as_ptr(), attrs, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES);
        if r == 0 || sfi.hIcon.is_null() {
            return None;
        }
        let img = icon_to_image(sfi.hIcon);
        DestroyIcon(sfi.hIcon);
        img
    }
}

/// # Safety
/// `hicon` must be a valid icon handle.
unsafe fn icon_to_image(hicon: *mut c_void) -> Option<ColorImage> {
    // SAFETY: ICONINFO/BITMAP are plain-old-data; all handles created here are released before returning.
    unsafe {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hicon, &mut ii) == 0 {
            return None;
        }
        let mut bm: BITMAP = std::mem::zeroed();
        let ok = !ii.hbmColor.is_null()
            && GetObjectW(ii.hbmColor, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void) != 0;
        let result = if ok && bm.bmWidth > 0 && bm.bmHeight > 0 {
            let (w, h) = (bm.bmWidth as usize, bm.bmHeight as usize);
            let hdc = CreateCompatibleDC(std::ptr::null_mut());
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut color = vec![0u8; w * h * 4];
            let got = GetDIBits(hdc, ii.hbmColor, 0, h as u32, color.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);
            let mut out = None;
            if got != 0 {
                if color.as_chunks::<4>().0.iter().all(|p| p[3] == 0) && !ii.hbmMask.is_null() {
                    // classic icon without alpha: the AND mask says which pixels are transparent
                    let mut mask = vec![0u8; w * h * 4];
                    let mut mb = bi;
                    GetDIBits(hdc, ii.hbmMask, 0, h as u32, mask.as_mut_ptr() as *mut c_void, &mut mb, DIB_RGB_COLORS);
                    for (c, m) in color.as_chunks_mut::<4>().0.iter_mut().zip(mask.as_chunks::<4>().0) {
                        c[3] = if m[0] == 0 { 255 } else { 0 };
                    }
                }
                for p in color.as_chunks_mut::<4>().0 {
                    p.swap(0, 2); // BGRA -> RGBA
                }
                out = Some(ColorImage::from_rgba_unmultiplied([w, h], &color));
            }
            DeleteDC(hdc);
            out
        } else {
            None
        };
        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor);
        }
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask);
        }
        result
    }
}

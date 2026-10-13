//! Helpers for `scripts/bench.ps1` (hidden from `--help`): test-data generators and the Explorer copy engine
//! (`IFileOperation`, exactly what Explorer drives) so Strela can be timed against it.

use std::fs::OpenOptions;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::Instant;

use clap::{Args, Subcommand};
use rayon::prelude::*;
use strela_core::transfer::aligned::AlignedBuf;
use strela_core::transfer::{Control, TransferProgress, copyfile2};

use super::Res;

#[derive(Args)]
pub struct BenchCmd {
    #[command(subcommand)]
    cmd: B,
}

#[derive(Subcommand)]
enum B {
    /// Один файл из несжимаемых данных, записанный без кэша (источник остаётся «холодным»)
    GenBig {
        path: PathBuf,
        #[arg(long)]
        mb: u64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Дерево мелких файлов
    GenSmall {
        dir: PathBuf,
        #[arg(long)]
        count: u64,
        #[arg(long, default_value_t = 1000)]
        dirs: u64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Копирование движком Проводника (IFileOperation, без окна прогресса)
    ShellCopy { src: PathBuf, dest_dir: PathBuf },
    ShellMove { src: PathBuf, dest_dir: PathBuf },
    /// Безвозвратное удаление движком Проводника
    ShellDelete { path: PathBuf },
    /// Чистый CopyFile2 для одного файла (без нашего конвейера)
    Cf2 { src: PathBuf, dst: PathBuf },
    /// Сбрасывает на диск (FlushFileBuffers) записанный файл: чтобы замеры учитывали и отложенную запись
    Flush { path: PathBuf },
    /// Число файлов и суммарный размер папки (для проверки результатов копирования)
    Count { dir: PathBuf },
    /// Показывает контекстное меню так, как его строит Проводник (проверка регистрации «Strela ▸»)
    MenuDump {
        path: PathBuf,
        /// Меню фона папки (правый клик по пустому месту внутри `path`)
        #[arg(long)]
        background: bool,
    },
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.as_chunks_mut::<8>().0 {
            *c = self.next().to_le_bytes();
        }
    }
}

pub fn run(b: BenchCmd) -> Res {
    match b.cmd {
        B::GenBig { path, mb, seed } => gen_big(path, mb, seed),
        B::GenSmall { dir, count, dirs, seed } => gen_small(dir, count, dirs, seed),
        B::ShellCopy { src, dest_dir } => shell(Shell::Copy, &src, &dest_dir),
        B::ShellMove { src, dest_dir } => shell(Shell::Move, &src, &dest_dir),
        B::ShellDelete { path } => shell(Shell::Delete, &path, &path),
        B::MenuDump { path, background } => menu_dump(&path, background),
        B::Flush { path } => {
            std::fs::OpenOptions::new().write(true).open(&path)?.sync_all()?;
            Ok(())
        }
        B::Count { dir } => {
            let (mut files, mut bytes, mut dirs) = (0u64, 0u64, 0u64);
            let mut stack = vec![dir];
            while let Some(d) = stack.pop() {
                let _ = strela_core::winapi::list_dir(&d, |e| {
                    use std::os::windows::ffi::OsStringExt;
                    if e.is_dir() {
                        dirs += 1;
                        stack.push(d.join(std::ffi::OsString::from_wide(e.name)));
                    } else {
                        files += 1;
                        bytes += e.size;
                    }
                });
            }
            println!("COUNT files={files} dirs={dirs} bytes={bytes}");
            Ok(())
        }
        B::Cf2 { src, dst } => {
            let size = std::fs::metadata(&src)?.len();
            let t = Instant::now();
            copyfile2::copy_with(&src, &dst, size, &Control::default(), &TransferProgress::default())?;
            println!("RESULT seconds={:.3} bytes={size}", t.elapsed().as_secs_f64());
            Ok(())
        }
    }
}

fn gen_big(path: PathBuf, mb: u64, seed: u64) -> Res {
    const BLOCK: usize = 8 << 20;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let total = mb << 20;
    let f = OpenOptions::new().write(true).create(true).truncate(true).custom_flags(0x2000_0000).open(&path)?;
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut buf = AlignedBuf::new(BLOCK);
    rng.fill(&mut buf);
    let mut off = 0u64;
    while off < total {
        // a cheap perturbation per block keeps blocks distinct without regenerating 8 MiB each time
        let n = ((off / BLOCK as u64) as usize * 8) % (BLOCK - 8);
        let v = rng.next().to_le_bytes();
        buf[n..n + 8].copy_from_slice(&v);
        let want = (total - off).min(BLOCK as u64) as usize;
        f.seek_write(&buf[..want.next_multiple_of(4096)], off)?;
        off += want as u64;
    }
    f.set_len(total)?;
    println!("создан {} ({} МБ)", path.display(), mb);
    Ok(())
}

fn gen_small(dir: PathBuf, count: u64, dirs: u64, seed: u64) -> Res {
    std::fs::create_dir_all(&dir)?;
    for d in 0..dirs {
        std::fs::create_dir_all(dir.join(format!("d{d:04}")))?;
    }
    let total: u64 = (0..count)
        .into_par_iter()
        .map(|i| {
            let mut rng = Rng((seed ^ (i + 1)).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            // skewed sizes: many tiny files, some tens of KiB
            let size = 128 + (rng.next() % ((rng.next() % 49_152) + 1)) as usize;
            let mut data = vec![0u8; size.next_multiple_of(8)];
            rng.fill(&mut data);
            data.truncate(size);
            let p = dir.join(format!("d{:04}", i % dirs)).join(format!("f{i:07}.dat"));
            std::fs::write(p, &data).expect("write test file");
            size as u64
        })
        .sum();
    println!("создано {count} файлов, {} МБ", total >> 20);
    Ok(())
}

enum Shell {
    Copy,
    Move,
    Delete,
}

fn shell(kind: Shell, src: &std::path::Path, dest_dir: &std::path::Path) -> Res {
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
    use windows::Win32::UI::Shell::{
        FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_NOERRORUI, FOF_SILENT, FileOperation, IFileOperation, IShellItem,
        SHCreateItemFromParsingName,
    };
    use windows::core::HSTRING;
    let item = |p: &std::path::Path| -> windows::core::Result<IShellItem> {
        // SAFETY: the HSTRING outlives the call.
        unsafe { SHCreateItemFromParsingName(&HSTRING::from(strela_core::winapi::display_path(p)), None) }
    };
    // SAFETY: COM is initialised once for this thread and released at the end.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let res = (|| -> windows::core::Result<f64> {
        // SAFETY: standard COM activation and calls on live interfaces.
        unsafe {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(FOF_NOCONFIRMATION | FOF_NOCONFIRMMKDIR | FOF_NOERRORUI | FOF_SILENT)?;
            let s = item(src)?;
            match kind {
                Shell::Copy => op.CopyItem(&s, &item(dest_dir)?, None, None)?,
                Shell::Move => op.MoveItem(&s, &item(dest_dir)?, None, None)?,
                Shell::Delete => op.DeleteItem(&s, None)?,
            }
            let t = Instant::now();
            op.PerformOperations()?;
            Ok(t.elapsed().as_secs_f64())
        }
    })();
    // SAFETY: matches CoInitializeEx above.
    unsafe { CoUninitialize() };
    println!("RESULT seconds={:.3}", res?);
    Ok(())
}

fn menu_dump(path: &std::path::Path, background: bool) -> Res {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        CMF_EXTENDEDVERBS, CMF_NORMAL, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, SHBindToParent, SHGetDesktopFolder, SHParseDisplayName,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreatePopupMenu, GetMenuItemCount, GetMenuItemInfoW, HMENU, MENUITEMINFOW, MFT_SEPARATOR, MIIM_FTYPE, MIIM_STRING, MIIM_SUBMENU, WM_INITMENUPOPUP,
    };
    use windows::core::Interface;

    fn dump(h: HMENU, ctx2: &IContextMenu2, depth: usize) {
        // SAFETY: menu handle owned by this function's caller; MENUITEMINFOW is filled by the system.
        unsafe {
            for i in 0..GetMenuItemCount(Some(h)).max(0) as u32 {
                let mut buf = [0u16; 256];
                let mut mii = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_STRING | MIIM_SUBMENU | MIIM_FTYPE,
                    dwTypeData: windows::core::PWSTR(buf.as_mut_ptr()),
                    cch: 255,
                    ..Default::default()
                };
                if GetMenuItemInfoW(h, i, true, &mut mii).is_err() {
                    continue;
                }
                if mii.fType.0 & MFT_SEPARATOR.0 != 0 {
                    continue;
                }
                let text = String::from_utf16_lossy(&buf[..mii.cch as usize]);
                println!("{}{}{}", "  ".repeat(depth), text, if mii.hSubMenu.is_invalid() { "" } else { "  ▸" });
                if !mii.hSubMenu.is_invalid() {
                    let _ = ctx2.HandleMenuMsg(WM_INITMENUPOPUP, WPARAM(mii.hSubMenu.0 as usize), LPARAM(i as isize));
                    if let Ok(c3) = ctx2.cast::<IContextMenu3>() {
                        let mut res = windows::Win32::Foundation::LRESULT(0);
                        let _ = c3.HandleMenuMsg2(WM_INITMENUPOPUP, WPARAM(mii.hSubMenu.0 as usize), LPARAM(i as isize), Some(&mut res));
                    }
                    // some menus (folder background) fill their sub-menus a moment later
                    for _ in 0..20 {
                        if GetMenuItemCount(Some(mii.hSubMenu)) > 0 {
                            break;
                        }
                        // let the shell's own windows and async work run
                        let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                        while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                            &mut msg,
                            None,
                            0,
                            0,
                            windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
                        )
                        .as_bool()
                        {
                            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    dump(mii.hSubMenu, ctx2, depth + 1);
                }
            }
        }
    }

    // SAFETY: COM calls on an initialised STA thread; PIDLs live until the end of the function.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let wide = windows::core::HSTRING::from(strela_core::winapi::display_path(path));
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(&wide, None, &mut pidl, 0, None)?;
        let menu: IContextMenu = if background {
            let desktop = SHGetDesktopFolder()?;
            let folder: IShellFolder = desktop.BindToObject(pidl, None)?;
            folder.CreateViewObject(HWND::default())?
        } else {
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            let parent: IShellFolder = SHBindToParent(pidl, Some(&mut child))?;
            parent.GetUIObjectOf(HWND::default(), &[child as *const ITEMIDLIST], None)?
        };
        let hmenu = CreatePopupMenu()?;
        menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL | CMF_EXTENDEDVERBS).ok()?;
        let ctx2: IContextMenu2 = menu.cast()?;
        dump(hmenu, &ctx2, 0);
        CoUninitialize();
    }
    Ok(())
}

//! Installer / uninstaller windows. The same executable becomes the installer when it is named `Strela-Setup.exe`
//! (or started with `--install`) and the uninstaller with `--uninstall`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Align, Id, Layout, RichText, Ui, vec2};

use super::theme::{self, pal};
use super::{App, Screen};
use crate::winint::{self, InstallOptions};

type InstallDone = Result<(PathBuf, Result<bool, String>), String>;

#[derive(Default)]
pub struct Install {
    opts: InstallOptions,
    job: Option<Receiver<InstallDone>>,
    result: Option<InstallDone>,
}

#[derive(Default)]
pub struct Uninstall {
    job: Option<Receiver<Result<(), String>>>,
    result: Option<Result<(), String>>,
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let logo = app.logo.clone();
    let ctx = ui.ctx().clone();
    let p = pal(ui);
    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::same(22))).show(ui, |ui| {
        let appear = ctx.animate_bool_with_time(Id::new("setup_in"), true, 0.3);
        ui.set_opacity(appear);
        ui.add_space((1.0 - appear) * 12.0);
        ui.vertical_centered(|ui| {
            if let Some(l) = &logo {
                ui.add(egui::Image::new(l).fit_to_exact_size(vec2(72.0, 72.0)));
            }
            ui.add_space(6.0);
            ui.label(RichText::new("Strela").size(26.0).strong());
            ui.label(theme::dim(ui, format!("версия {}", env!("CARGO_PKG_VERSION"))));
        });
        ui.add_space(14.0);
        match &mut app.screen {
            Screen::Install(s) => install_body(ui, s),
            Screen::Uninstall(s) => uninstall_body(ui, s),
            _ => {}
        }
    });
}

fn install_body(ui: &mut Ui, s: &mut Install) {
    let p = pal(ui);
    if let Some(rx) = &s.job {
        if let Ok(r) = rx.try_recv() {
            s.result = Some(r);
            s.job = None;
        } else {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.add(egui::Spinner::new().size(34.0).color(p.accent));
                ui.label("Установка…");
                ui.label(theme::dim(ui, "Если вы выбрали службу индексации, подтвердите запрос Windows."));
            });
            ui.ctx().request_repaint();
            return;
        }
    }
    match &s.result {
        Some(Ok((exe, service))) => {
            let exe = exe.clone();
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("✔ Готово").size(20.0).color(p.ok).strong());
                ui.add_space(4.0);
                ui.label("Strela установлена. Она запускается в трее, а меню «Strela ▸» появилось в Проводнике");
                ui.label("(в Windows 11 — в «Показать дополнительные параметры» или Shift+F10).");
                match service {
                    Ok(true) => ui.colored_label(p.ok, "Служба индексации установлена: поиск по всей системе — мгновенный."),
                    Ok(false) => ui.label(theme::dim(ui, "Служба индексации не устанавливалась — поиск работает через обход папок.")),
                    Err(e) => ui.colored_label(p.warn, format!("Служба индексации не установлена ({e}). Её можно добавить позже в настройках.")),
                };
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 250.0).max(0.0) / 2.0);
                    if theme::primary_button(ui, "Запустить Strela").clicked() {
                        let _ = std::process::Command::new(&exe).spawn();
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if ui.button("Закрыть").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        }
        Some(Err(e)) => {
            ui.label(RichText::new("Не удалось установить").strong().color(p.err));
            ui.label(e.as_str());
            if ui.button("Попробовать ещё раз").clicked() {
                s.result = None;
            }
        }
        None => {
            let dir = winint::install_dir().map(|d| d.display().to_string()).unwrap_or_default();
            let upgrade = winint::installed_exe().is_some_and(|e| e.exists());
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(theme::dim(ui, "Папка установки"));
                ui.label(dir);
                ui.add_space(8.0);
                ui.checkbox(&mut s.opts.context_menu, "Меню «Strela ▸» в контекстном меню Проводника");
                ui.checkbox(&mut s.opts.send_to, "Пункты «Отправить» для переноса на другой диск");
                ui.checkbox(&mut s.opts.autostart, "Запускать при входе в Windows (в трей)");
                ui.checkbox(&mut s.opts.desktop_shortcut, "Ярлык на рабочем столе");
                ui.checkbox(&mut s.opts.service, "Служба индексации (мгновенный поиск по MFT; один запрос прав администратора)");
                ui.add_space(4.0);
                ui.label(theme::dim(ui, "Ярлык в меню «Пуск» создаётся всегда. Без службы всё работает — поиск идёт через обход папок."));
            });
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Отмена").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if theme::primary_button(ui, if upgrade { "Обновить" } else { "Установить" }).clicked() {
                    let (tx, rx) = channel();
                    let opts = s.opts;
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(winint::install(opts).map(|r| (r.exe, r.service)));
                        ctx.request_repaint();
                    });
                    s.job = Some(rx);
                }
            });
        }
    }
}

fn uninstall_body(ui: &mut Ui, s: &mut Uninstall) {
    let p = pal(ui);
    if let Some(rx) = &s.job {
        if let Ok(r) = rx.try_recv() {
            s.result = Some(r);
            s.job = None;
        } else {
            ui.vertical_centered(|ui| {
                ui.add(egui::Spinner::new().size(30.0).color(p.accent));
                ui.label("Удаление…");
            });
            ui.ctx().request_repaint();
            return;
        }
    }
    match &s.result {
        None => {
            ui.vertical_centered(|ui| {
                ui.label("Удалить Strela с этого компьютера?");
                ui.label(theme::dim(ui, "Будут удалены программа, ярлыки, меню Проводника, пункты «Отправить», автозапуск, служба и настройки."));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 190.0).max(0.0) / 2.0);
                    if theme::danger_button(ui, "Удалить").clicked() {
                        let (tx, rx) = channel();
                        let ctx = ui.ctx().clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(winint::uninstall());
                            ctx.request_repaint();
                        });
                        s.job = Some(rx);
                    }
                    if ui.button("Отмена").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        }
        Some(Ok(())) => {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Strela удалена").size(18.0).strong());
                ui.add_space(10.0);
                if ui.button("Закрыть").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        }
        Some(Err(e)) => {
            ui.label(RichText::new("Удалено с замечанием").strong().color(p.warn));
            ui.label(e.as_str());
            if ui.button("Закрыть").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

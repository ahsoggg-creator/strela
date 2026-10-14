//! "Настройки".

use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;

use eframe::egui::{self, Color32, Key, RichText, Ui, vec2};
use strela_core::transfer::HashAlgo;

use super::theme::{self, pal};
use crate::backend::{Mode, Status};
use crate::settings::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, ThemeChoice};
use crate::state::Shared;
use crate::{service, winint};

const ACCENTS: [([u8; 3], &str); 8] = [
    ([45, 160, 235], "Небо"),
    ([45, 212, 191], "Бирюза"),
    ([74, 200, 120], "Зелень"),
    ([235, 175, 60], "Янтарь"),
    ([240, 120, 70], "Закат"),
    ([235, 90, 120], "Роза"),
    ([150, 110, 235], "Сирень"),
    ([120, 130, 150], "Графит"),
];

pub struct PrefsTab {
    capturing: bool,
    message: Option<(String, bool)>,
    job: Option<Receiver<Result<String, String>>>,
    new_root: String,
}

pub fn vk_of(k: Key) -> Option<u32> {
    use Key::*;
    Some(match k {
        Space => 0x20,
        Enter => 0x0D,
        Tab => 0x09,
        Num0 => 0x30,
        Num1 => 0x31,
        Num2 => 0x32,
        Num3 => 0x33,
        Num4 => 0x34,
        Num5 => 0x35,
        Num6 => 0x36,
        Num7 => 0x37,
        Num8 => 0x38,
        Num9 => 0x39,
        A => 0x41,
        B => 0x42,
        C => 0x43,
        D => 0x44,
        E => 0x45,
        F => 0x46,
        G => 0x47,
        H => 0x48,
        I => 0x49,
        J => 0x4A,
        K => 0x4B,
        L => 0x4C,
        M => 0x4D,
        N => 0x4E,
        O => 0x4F,
        P => 0x50,
        Q => 0x51,
        R => 0x52,
        S => 0x53,
        T => 0x54,
        U => 0x55,
        V => 0x56,
        W => 0x57,
        X => 0x58,
        Y => 0x59,
        Z => 0x5A,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        _ => return None,
    })
}

impl PrefsTab {
    pub fn new() -> PrefsTab {
        PrefsTab { capturing: false, message: None, job: None, new_root: String::new() }
    }

    /// Returns true when look & feel changed (theme / accent) and styles must be rebuilt.
    pub fn ui(&mut self, ui: &mut Ui, shared: &Arc<Shared>, status: &Status) -> bool {
        let p = pal(ui);
        let mut visuals_changed = false;
        let mut s = shared.settings();
        let before = s.clone();
        if let Some(rx) = &self.job
            && let Ok(r) = rx.try_recv()
        {
            self.message = Some(match r {
                Ok(m) => (m, false),
                Err(m) => (m, true),
            });
            self.job = None;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(900.0));
            if let Some((m, err)) = &self.message {
                ui.colored_label(if *err { p.err } else { p.ok }, m);
            }
            // ---- appearance
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::heading(ui, "Внешний вид");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut s.theme, ThemeChoice::System, "Как в системе");
                    ui.selectable_value(&mut s.theme, ThemeChoice::Dark, "Тёмная");
                    ui.selectable_value(&mut s.theme, ThemeChoice::Light, "Светлая");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Акцентный цвет:");
                    for (c, name) in ACCENTS {
                        let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), egui::Sense::click());
                        let col = Color32::from_rgb(c[0], c[1], c[2]);
                        ui.painter().circle_filled(rect.center(), 11.0, col);
                        if s.accent == c {
                            ui.painter().circle_stroke(rect.center(), 13.5, egui::Stroke::new(2.0, p.text));
                        }
                        if resp.on_hover_text(name).clicked() {
                            s.accent = c;
                        }
                    }
                    let mut rgb = s.accent;
                    if ui.color_edit_button_srgb(&mut rgb).changed() {
                        s.accent = rgb;
                    }
                });
            });
            ui.add_space(10.0);
            // ---- hotkey
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::heading(ui, "Быстрый поиск (как Spotlight)");
                ui.label(theme::dim(ui, "Глобальное сочетание открывает всплывающее окно поиска из любой программы."));
                ui.horizontal(|ui| {
                    if self.capturing {
                        ui.label(RichText::new("Нажмите сочетание клавиш… (Esc — отмена)").color(p.accent));
                        let (mods, key, esc) = ui.input(|i| {
                            let key = i.events.iter().find_map(|e| match e {
                                egui::Event::Key { key, pressed: true, .. } => Some(*key),
                                _ => None,
                            });
                            (i.modifiers, key, i.key_pressed(Key::Escape))
                        });
                        if esc {
                            self.capturing = false;
                        } else if let Some(vk) = key.and_then(vk_of) {
                            let mut m = 0;
                            if mods.ctrl {
                                m |= MOD_CONTROL;
                            }
                            if mods.alt {
                                m |= MOD_ALT;
                            }
                            if mods.shift {
                                m |= MOD_SHIFT;
                            }
                            if mods.mac_cmd || mods.command && !mods.ctrl {
                                m |= MOD_WIN;
                            }
                            if m != 0 {
                                s.hotkey = Hotkey { mods: m, vk };
                                self.capturing = false;
                            }
                        }
                    } else {
                        ui.label(RichText::new(s.hotkey.describe()).strong().size(16.0));
                        if ui.button("Изменить").clicked() {
                            self.capturing = true;
                        }
                        if ui.button("По умолчанию").clicked() {
                            s.hotkey = Hotkey::default();
                        }
                    }
                });
                if let Some(e) = shared.hotkey_error.lock().unwrap().as_ref() {
                    ui.colored_label(p.err, e);
                }
            });
            ui.add_space(10.0);
            // ---- updates
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::heading(ui, "Обновления");
                ui.checkbox(&mut s.check_updates, "Проверять обновления на GitHub (раз в сутки)");
                let state = shared.update.lock().unwrap().clone();
                ui.horizontal(|ui| {
                    ui.label(format!("Установлена версия {}", crate::update::CURRENT));
                    match &state {
                        crate::update::UpdateState::Checking => {
                            ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                        }
                        crate::update::UpdateState::UpToDate => {
                            ui.colored_label(p.ok, "· это последняя версия");
                        }
                        crate::update::UpdateState::Available(r) => {
                            ui.colored_label(p.ok, format!("· доступна {}", r.version));
                        }
                        crate::update::UpdateState::Failed(e) => {
                            ui.colored_label(p.err, format!("· {e}"));
                        }
                        _ => {}
                    }
                    if ui.add_enabled(!matches!(state, crate::update::UpdateState::Checking | crate::update::UpdateState::Installing), egui::Button::new("Проверить сейчас")).clicked() {
                        let sh = shared.clone();
                        std::thread::spawn(move || crate::update::check_now(&sh));
                    }
                });
                ui.label(theme::dim(ui, "Единственный сетевой запрос Strela — к api.github.com (последний релиз); отправляются только название программы и версия. Файл скачивается с GitHub и сверяется с SHA256SUMS.txt."));
            });
            ui.add_space(10.0);
            // ---- startup & Explorer integration
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::heading(ui, "Запуск и Проводник");
                ui.checkbox(&mut s.autostart, "Запускать при входе в Windows (в трей)");
                ui.checkbox(&mut s.close_to_tray, "Кнопка закрытия сворачивает в трей");
                let installed = winint::running_installed();
                let mut menu = winint::context_menu_enabled();
                if ui.checkbox(&mut menu, "Меню «Strela ▸» в контекстном меню Проводника").changed() {
                    let exe = if installed { std::env::current_exe().unwrap_or_default() } else { winint::installed_exe().unwrap_or_default() };
                    self.message = Some(match winint::set_context_menu(menu, &exe) {
                        Ok(()) => (if menu { "Меню добавлено".into() } else { "Меню убрано".into() }, false),
                        Err(e) => (e, true),
                    });
                }
                ui.label(theme::dim(ui, "В Windows 11 пункт находится в «Показать дополнительные параметры» (или Shift+F10)."));
                let mut sendto = winint::sendto_dir().is_some_and(|d| {
                    std::fs::read_dir(d).is_ok_and(|r| r.flatten().any(|e| e.file_name().to_string_lossy().starts_with("Strela - ")))
                });
                if ui.checkbox(&mut sendto, "Пункты «Отправить» для переноса на другой диск").changed() {
                    let exe = std::env::current_exe().unwrap_or_default();
                    self.message = Some(match winint::sync_sendto(&exe, sendto) {
                        Ok(()) => ("Готово".into(), false),
                        Err(e) => (e, true),
                    });
                }
                if !installed {
                    ui.add_space(4.0);
                    ui.label(theme::dim(ui, "Это переносная версия. Установка добавит ярлыки, меню Проводника и автозапуск."));
                    if self.job.is_none() && ui.button("Установить Strela в систему…").clicked() {
                        let (tx, rx) = channel();
                        std::thread::spawn(move || {
                            let r = winint::install(winint::InstallOptions::default())
                                .map(|r| format!("Установлено в {}", r.exe.parent().map(|d| d.display().to_string()).unwrap_or_default()));
                            let _ = tx.send(r);
                        });
                        self.job = Some(rx);
                    }
                }
            });
            ui.add_space(10.0);
            // ---- indexing
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::heading(ui, "Индексация");
                let (mode_txt, mode_col) = match status.mode {
                    Mode::Service => ("Служба StrelaIndexer: чтение MFT + журнал USN (мгновенно после перезагрузки)", p.ok),
                    Mode::Local => ("Встроенный индекс: параллельный обход папок + слежение за изменениями", p.warn),
                };
                ui.colored_label(mode_col, mode_txt);
                for r in &status.roots {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&r.root).strong());
                        ui.label(theme::dim(ui, format!("{} записей · {}", r.entries, if r.mft { "MFT" } else { "обход" })));
                        let t = match r.phase.as_str() {
                            "ready" => "готов".to_string(),
                            "scanning" => format!("сканирование {:.0}%", r.progress * 100.0),
                            "loading" => "загрузка".to_string(),
                            other => other.to_string(),
                        };
                        ui.label(theme::dim(ui, t));
                    });
                }
                ui.horizontal(|ui| {
                    if ui.button("Пересканировать").clicked() {
                        shared.backend.rescan();
                    }
                    ui.checkbox(&mut s.use_service, "Использовать службу, если она запущена");
                });
                let installed = service::is_installed();
                ui.horizontal(|ui| {
                    if installed {
                        ui.label(theme::dim(ui, if service::is_running() { "Служба установлена и запущена" } else { "Служба установлена, но не запущена" }));
                        if self.job.is_none() && ui.button("Удалить службу (запрос прав)").clicked() {
                            let (tx, rx) = channel();
                            std::thread::spawn(move || {
                                let _ = tx.send(service::uninstall_via_uac().map(|_| "Служба удалена".to_string()));
                            });
                            self.job = Some(rx);
                        }
                    } else {
                        ui.label(theme::dim(ui, "Служба не установлена: поиск работает через обход папок."));
                        if self.job.is_none() && ui.button("Установить службу индексации (запрос прав)").clicked() {
                            let exe = std::env::current_exe().unwrap_or_default();
                            let (tx, rx) = channel();
                            std::thread::spawn(move || {
                                let _ = tx.send(service::install_via_uac(&exe).map(|_| "Служба установлена и запущена".to_string()));
                            });
                            self.job = Some(rx);
                        }
                    }
                });
                if status.mode == Mode::Local {
                    ui.add_space(4.0);
                    ui.label("Папки для встроенного индекса (пусто — все локальные диски):");
                    let mut remove = None;
                    for (i, r) in s.index_roots.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.small_button("✕").clicked() {
                                remove = Some(i);
                            }
                            ui.label(r);
                        });
                    }
                    if let Some(i) = remove {
                        s.index_roots.remove(i);
                    }
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.new_root).hint_text(r"D:\Games").desired_width(260.0).margin(vec2(10.0, 6.0)));
                        if ui.button("Добавить").clicked() && !self.new_root.trim().is_empty() {
                            s.index_roots.push(self.new_root.trim().trim_matches('"').to_string());
                            self.new_root.clear();
                        }
                        if ui.button("Обзор…").clicked()
                            && let Some(f) = rfd::FileDialog::new().pick_folder()
                        {
                            s.index_roots.push(f.to_string_lossy().into_owned());
                        }
                    });
                }
            });
            ui.add_space(10.0);
            // ---- transfers and editor
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::heading(ui, "Передача и открытие");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.verify_copies, "Проверять целостность копий по умолчанию");
                    if s.verify_copies {
                        ui.selectable_value(&mut s.hash, HashAlgo::Blake3, "BLAKE3");
                        ui.selectable_value(&mut s.hash, HashAlgo::Xxh3, "xxh3");
                    }
                });
                ui.checkbox(&mut s.fuzzy, "Нечёткий поиск по именам");
                ui.checkbox(&mut s.regex, "Запрос поиска — регулярное выражение");
                ui.add_space(4.0);
                ui.label("Команда открытия файла на нужной строке (поиск по содержимому):");
                ui.add(egui::TextEdit::singleline(&mut s.editor).hint_text("пусто — VS Code / Notepad++, если найдены, иначе программа по умолчанию").desired_width(560.0).margin(vec2(10.0, 6.0)));
                ui.horizontal(|ui| {
                    if ui.small_button("VS Code").clicked() {
                        s.editor = "code -g \"{path}:{line}\"".into();
                    }
                    if ui.small_button("Notepad++").clicked() {
                        s.editor = "\"C:\\Program Files\\Notepad++\\notepad++.exe\" -n{line} \"{path}\"".into();
                    }
                    if ui.small_button("Сбросить").clicked() {
                        s.editor.clear();
                    }
                    ui.label(theme::dim(ui, "{path} и {line} подставляются"));
                });
            });
            ui.add_space(10.0);
            theme::card(ui).show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::heading(ui, "О программе");
                ui.label(format!("Strela {} — быстрые файловые операции для Windows", env!("CARGO_PKG_VERSION")));
                ui.label(theme::dim(ui, "Никакой телеметрии. Единственный сетевой запрос — проверка обновлений на GitHub (настройка выше)."));
                ui.hyperlink_to("Исходный код и релизы на GitHub", winint::REPO_URL);
                ui.label(theme::dim(ui, format!("Данные программы: {}", crate::settings::dir().display())));
            });
        });
        if s.theme != before.theme || s.accent != before.accent {
            visuals_changed = true;
        }
        let changed = format!("{s:?}") != format!("{before:?}");
        if changed {
            if s.autostart != before.autostart {
                let _ = winint::set_autostart(s.autostart);
            }
            let hotkey_changed = s.hotkey != before.hotkey;
            *shared.settings.write().unwrap() = s.clone();
            s.save();
            if hotkey_changed {
                crate::shell::reregister_hotkey(shared);
            }
            if s.use_service != before.use_service || s.index_roots != before.index_roots {
                shared.backend.apply_settings(&s);
            }
        }
        visuals_changed
    }
}

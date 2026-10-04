//! Consent to error reports, "Сообщить о проблеме" and in-app updates.

use std::path::PathBuf;
use std::time::Duration;

use slint::ComponentHandle;

use crate::update::{self, Found, Install};
use crate::{AppWindow, Shared, diag, save};

/// Where an update stands; shown as a banner in the sidebar.
#[derive(Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    Downloading(String),
    /// Verified and ready to install.
    Ready(Found, PathBuf),
    /// Newer version without a build we can install here: link to the page.
    Notify(Found),
}

/// First check a little after start, then every 6 hours.
const FIRST_CHECK: Duration = Duration::from_secs(30);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

pub fn install(ui: &AppWindow, app: &Shared) {
    {
        let st = app.lock().unwrap();
        let s = &st.store.settings;
        diag::configure(s.telemetry == Some(true), &s.install_id);
        ui.set_consent_open(s.telemetry.is_none());
    }
    diag::flush();

    ui.on_consent({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |agree| {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            set_consent(&mut st.store.settings, agree);
            save(&mut st);
            ui.set_telemetry_enabled(agree);
            ui.set_consent_open(false);
        }
    });

    ui.on_report_problem({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            ui.set_report_note("".into());
            ui.set_report_preview("".into());
            ui.set_report_code("".into());
            ui.set_report_error("".into());
            ui.set_report_open(true);
        }
    });
    ui.on_report_preview_requested({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            let report = diag::manual_report(&ui.get_report_note());
            ui.set_report_preview(serde_json::to_string_pretty(&report).unwrap_or_default().into());
        }
    });
    ui.on_report_send({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            let report = diag::manual_report(&ui.get_report_note());
            ui.set_report_busy(true);
            ui.set_report_error("".into());
            let ui_weak = ui.as_weak();
            std::thread::spawn(move || {
                let result = diag::send_now(&report);
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_report_busy(false);
                    match result {
                        Ok(code) => {
                            diag::log(format!("report sent: {code}"));
                            ui.set_report_code(code.into());
                        }
                        Err(e) => ui.set_report_error(format!("Не удалось отправить: {e:#}").into()),
                    }
                });
            });
        }
    });
    ui.on_report_copy_code({
        let ui_weak = ui.as_weak();
        move || {
            let code = ui_weak.unwrap().get_report_code().to_string();
            let _ = arboard::Clipboard::new().and_then(|mut c| c.set_text(code));
        }
    });

    ui.on_check_update({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || check(&ui_weak.unwrap(), &app, true)
    });
    ui.on_update_clicked({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || apply(&ui_weak.unwrap(), &app)
    });

    let (ui_weak, app_) = (ui.as_weak(), app.clone());
    slint::Timer::single_shot(FIRST_CHECK, move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        check(&ui, &app_, false);
        let (ui_weak, app_) = (ui.as_weak(), app_.clone());
        let timer = Box::leak(Box::new(slint::Timer::default()));
        timer.start(slint::TimerMode::Repeated, CHECK_EVERY, move || {
            if let Some(ui) = ui_weak.upgrade() {
                check(&ui, &app_, false);
            }
            // Reports queued while offline get another chance as well.
            diag::flush();
        });
    });
    show(ui, &app.lock().unwrap().update);
}

/// Records the answer; the install id exists only while reports are on, and
/// opting out drops reports that were still waiting to be sent.
pub fn set_consent(s: &mut duoray_core::store::Settings, agree: bool) {
    if agree && s.install_id.is_empty() {
        s.install_id = format!("{:016x}", fastrand::u64(..));
    }
    if !agree {
        s.install_id.clear();
        diag::drop_queue();
    }
    s.telemetry = Some(agree);
    diag::configure(agree, &s.install_id);
    diag::log(format!("reports {}", if agree { "on" } else { "off" }));
}

fn check(ui: &AppWindow, app: &Shared, manual: bool) {
    {
        let mut st = app.lock().unwrap();
        let busy = matches!(st.update, UpdateState::Checking | UpdateState::Downloading(_));
        if busy || (!manual && !st.store.settings.auto_update) || matches!(st.update, UpdateState::Ready(..)) {
            return;
        }
        st.update = UpdateState::Checking;
        show(ui, &st.update);
    }
    ui.set_update_info("Проверка обновлений…".into());
    let dir = crate::routing_ui::data_dir(app).join("updates");
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let found = update::check();
        let shown = found.as_ref().map(Clone::clone).map_err(|e| format!("{e:#}"));
        let _ = ui_weak.upgrade_in_event_loop({
            let app = app.clone();
            move |ui| {
                let mut st = app.lock().unwrap();
                match &shown {
                    Ok(None) => {
                        st.update = UpdateState::Idle;
                        ui.set_update_info(format!("Установлена последняя версия ({}).", env!("CARGO_PKG_VERSION")).into());
                    }
                    Ok(Some(f)) if f.install == Install::Notify || f.asset.is_none() => {
                        diag::log(format!("update {} available (no build for this install)", f.version));
                        ui.set_update_info(format!("Доступна версия {}.", f.version).into());
                        st.update = UpdateState::Notify(f.clone());
                    }
                    Ok(Some(f)) => {
                        diag::log(format!("update {} found, downloading", f.version));
                        ui.set_update_info(format!("Загружается версия {}…", f.version).into());
                        st.update = UpdateState::Downloading(f.version.clone());
                    }
                    Err(e) => {
                        st.update = UpdateState::Idle;
                        diag::log(format!("update check failed: {e}"));
                        ui.set_update_info(format!("Не удалось проверить обновления: {e}").into());
                    }
                }
                show(&ui, &st.update);
            }
        });
        let Ok(Some(found)) = found else { return };
        let Some(asset) = found.asset.clone().filter(|_| found.install != Install::Notify) else { return };
        let last = std::sync::atomic::AtomicU64::new(0);
        let result = update::download(&asset, &found.hub, &dir, |done, total| {
            // A few progress updates, not one per chunk.
            let pct = done * 100 / total.max(1);
            if pct >= last.load(std::sync::atomic::Ordering::Relaxed) + 5 {
                last.store(pct, std::sync::atomic::Ordering::Relaxed);
                let ui_weak = ui_weak.clone();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_update_banner(format!("Загрузка обновления… {pct}%").into()));
            }
        });
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            match result {
                Ok(file) => {
                    diag::log(format!("update {} downloaded and verified", found.version));
                    let notes = if found.notes.is_empty() { String::new() } else { format!(" Что нового: {}", found.notes) };
                    ui.set_update_info(format!("Версия {} загружена и проверена.{notes}", found.version).into());
                    st.update = UpdateState::Ready(found, file);
                }
                Err(e) => {
                    diag::log(format!("update download failed: {e:#}"));
                    ui.set_update_info(format!("Не удалось загрузить обновление: {e:#}").into());
                    st.update = UpdateState::Idle;
                }
            }
            show(&ui, &st.update);
        });
    });
}

fn show(ui: &AppWindow, state: &UpdateState) {
    let (banner, button) = match state {
        UpdateState::Idle | UpdateState::Checking => (String::new(), ""),
        UpdateState::Downloading(v) => (format!("Загрузка обновления {v}…"), ""),
        UpdateState::Ready(f, _) => (
            format!("Доступно обновление {}", f.version),
            if f.install == Install::Open { "Открыть" } else { "Установить" },
        ),
        UpdateState::Notify(f) => (format!("Доступно обновление {}", f.version), if f.page.is_empty() { "" } else { "Скачать" }),
    };
    ui.set_update_banner(banner.into());
    ui.set_update_button(button.into());
}

fn apply(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    match std::mem::take(&mut st.update) {
        UpdateState::Notify(f) => {
            crate::open_url(&f.page);
            st.update = UpdateState::Notify(f);
        }
        UpdateState::Ready(f, file) => {
            let quits = matches!(f.install, Install::Setup | Install::AppImage(_));
            diag::log(format!("installing update {}", f.version));
            // Bring the network back before the installer stops this app.
            let conn = if quits { st.conn.take() } else { None };
            if conn.is_some() {
                st.conn_state = crate::ConnState::Idle;
            }
            ui.set_update_banner("Установка обновления…".into());
            ui.set_update_button("".into());
            drop(st);
            let (ui_weak, app) = (ui.as_weak(), app.clone());
            std::thread::spawn(move || {
                if let Some(c) = conn {
                    c.disconnect();
                }
                let result = update::install(&file, &f.install);
                let _ = ui_weak.upgrade_in_event_loop(move |ui| match result {
                    Ok(()) if quits => {
                        let _ = slint::quit_event_loop();
                    }
                    Ok(()) => {
                        let mut st = app.lock().unwrap();
                        st.update = UpdateState::Ready(f, file);
                        show(&ui, &st.update);
                    }
                    Err(e) => {
                        diag::auto("update_install", &format!("{e:#}"));
                        ui.set_update_info(format!("Не удалось установить обновление: {e:#}").into());
                        let mut st = app.lock().unwrap();
                        st.update = UpdateState::Ready(f, file);
                        show(&ui, &st.update);
                    }
                });
            });
        }
        other => st.update = other,
    }
}

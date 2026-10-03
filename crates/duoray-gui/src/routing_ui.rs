//! The "Маршрутизация" page: simple switches, advanced profiles, geo databases.
//!
//! Every edit is saved at once (debounced); if connected, the connection is
//! restarted a moment later, because the rules live in xray's config.

use std::rc::Rc;
use std::time::Duration;

use duoray_core::geo::GeoDir;
use duoray_core::routing::{Action, AppMode, GAMES, RouteProfile, RouteRule, RoutingSettings};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{AppWindow, ConnState, GameRow, RuleRow, Shared, connection, save};

const STRATEGY_PANEL: &str = "Как в подписке";
const NETWORKS: [&str; 3] = ["", "tcp", "udp"];

pub fn data_dir(app: &Shared) -> std::path::PathBuf {
    let st = app.lock().unwrap();
    st.path.parent().map(std::path::Path::to_path_buf).unwrap_or_default()
}

pub fn geo_dir(app: &Shared) -> GeoDir {
    GeoDir::new(&data_dir(app))
}

/// Fills the page from the settings.
pub fn load(ui: &AppWindow, s: &RoutingSettings) {
    ui.set_routing_advanced(s.advanced);
    ui.set_routing_lan(s.simple.lan_direct);
    ui.set_routing_ru(s.simple.ru_direct);
    ui.set_routing_whitelist(s.simple.whitelist_direct);
    let games: Vec<GameRow> = GAMES
        .iter()
        .map(|g| GameRow {
            id: g.id.into(),
            title: g.title.into(),
            subtitle: g.subtitle.into(),
            on: s.simple.games.iter().any(|id| id == g.id),
        })
        .collect();
    ui.set_routing_games(ModelRc::new(VecModel::from(games)));
    ui.set_routing_app_mode(APP_MODES.iter().position(|m| *m == s.apps.mode).unwrap_or(0) as i32);
    load_apps(ui, s);
    load_profile(ui, s);
}

const APP_MODES: [AppMode; 3] = [AppMode::Off, AppMode::Bypass, AppMode::Only];

fn load_apps(ui: &AppWindow, s: &RoutingSettings) {
    let apps: Vec<slint::SharedString> = s.apps.apps.iter().map(|a| a.as_str().into()).collect();
    ui.set_routing_apps(ModelRc::new(VecModel::from(apps)));
}

fn load_profile(ui: &AppWindow, s: &RoutingSettings) {
    let names: Vec<slint::SharedString> = s.profiles.iter().map(|p| p.name.as_str().into()).collect();
    ui.set_routing_profiles(ModelRc::new(VecModel::from(names)));
    let index = s.active_profile.min(s.profiles.len().saturating_sub(1));
    ui.set_routing_profile_index(index as i32);
    let p = s.profile().cloned().unwrap_or_default();
    ui.set_routing_profile_name(p.name.as_str().into());
    ui.set_routing_default_action(Action::ALL.iter().position(|a| *a == p.default_action).unwrap_or(0) as i32);
    ui.set_routing_keep_panel(p.keep_panel_rules);
    ui.set_routing_domain_strategy(
        if p.domain_strategy.is_empty() { STRATEGY_PANEL } else { p.domain_strategy.as_str() }.into(),
    );
    let rules: Vec<RuleRow> = p.rules.iter().map(rule_row).collect();
    ui.set_routing_rules(ModelRc::new(VecModel::from(rules)));
}

fn rule_row(r: &RouteRule) -> RuleRow {
    RuleRow {
        enabled: r.enabled,
        action: Action::ALL.iter().position(|a| *a == r.action).unwrap_or(0) as i32,
        domains: r.domains.as_str().into(),
        ips: r.ips.as_str().into(),
        ports: r.ports.as_str().into(),
        network: NETWORKS.iter().position(|n| *n == r.network).unwrap_or(0) as i32,
    }
}

fn rule_of(row: &RuleRow) -> RouteRule {
    RouteRule {
        enabled: row.enabled,
        action: Action::ALL.get(row.action as usize).copied().unwrap_or_default(),
        domains: row.domains.to_string(),
        ips: row.ips.to_string(),
        ports: row.ports.to_string(),
        network: NETWORKS.get(row.network as usize).copied().unwrap_or_default().to_string(),
    }
}

/// Reads the switches and the profile fields (not the rule list, which is
/// updated per edit).
fn read_fields(ui: &AppWindow, s: &mut RoutingSettings) {
    s.advanced = ui.get_routing_advanced();
    s.simple.lan_direct = ui.get_routing_lan();
    s.simple.ru_direct = ui.get_routing_ru();
    s.simple.whitelist_direct = ui.get_routing_whitelist();
    s.apps.mode = APP_MODES.get(ui.get_routing_app_mode() as usize).copied().unwrap_or_default();
    if s.profiles.is_empty() {
        s.profiles.push(RouteProfile::default());
    }
    let i = s.active_profile.min(s.profiles.len() - 1);
    let p = &mut s.profiles[i];
    let name = ui.get_routing_profile_name().trim().to_string();
    if !name.is_empty() {
        p.name = name;
    }
    p.default_action = Action::ALL.get(ui.get_routing_default_action() as usize).copied().unwrap_or_default();
    p.keep_panel_rules = ui.get_routing_keep_panel();
    let strategy = ui.get_routing_domain_strategy().to_string();
    p.domain_strategy = if strategy == STRATEGY_PANEL { String::new() } else { strategy };
}

fn active_profile(s: &mut RoutingSettings) -> &mut RouteProfile {
    if s.profiles.is_empty() {
        s.profiles.push(RouteProfile::default());
    }
    let i = s.active_profile.min(s.profiles.len() - 1);
    s.active_profile = i;
    &mut s.profiles[i]
}

pub fn install(ui: &AppWindow, app: &Shared) {
    {
        let st = app.lock().unwrap();
        load(ui, &st.store.settings.routing);
    }
    // Save quickly; reconnect only after the user stops editing.
    let save_timer = Rc::new(slint::Timer::default());
    let reconnect_timer = Rc::new(slint::Timer::default());
    let committed: Rc<dyn Fn(&AppWindow)> = {
        let (app, save_timer, reconnect_timer) = (app.clone(), save_timer.clone(), reconnect_timer.clone());
        Rc::new(move |ui: &AppWindow| {
            let app2 = app.clone();
            save_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(400), move || {
                save(&mut app2.lock().unwrap());
            });
            let (ui_weak, app2) = (ui.as_weak(), app.clone());
            reconnect_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(1500), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let connected = matches!(app2.lock().unwrap().conn_state, ConnState::Connected { .. });
                    if connected {
                        crate::reconnect(&ui, &app2);
                    }
                }
            });
        })
    };

    ui.on_routing_changed({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move || {
            let ui = ui_weak.unwrap();
            {
                let mut st = app.lock().unwrap();
                read_fields(&ui, &mut st.store.settings.routing);
                // A renamed profile shows up in the picker.
                let names: Vec<slint::SharedString> =
                    st.store.settings.routing.profiles.iter().map(|p| p.name.as_str().into()).collect();
                let model = ui.get_routing_profiles();
                if model.row_count() == names.len() {
                    for (i, n) in names.into_iter().enumerate() {
                        if model.row_data(i).as_ref() != Some(&n) {
                            model.set_row_data(i, n);
                        }
                    }
                }
            }
            committed(&ui);
        }
    });

    ui.on_routing_game_toggled({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move |i, on| {
            let ui = ui_weak.unwrap();
            let Some(game) = GAMES.get(i as usize) else { return };
            {
                let mut st = app.lock().unwrap();
                let games = &mut st.store.settings.routing.simple.games;
                games.retain(|g| g != game.id);
                if on {
                    games.push(game.id.to_string());
                }
            }
            let model = ui.get_routing_games();
            if let Some(mut row) = model.row_data(i as usize) {
                row.on = on;
                model.set_row_data(i as usize, row);
            }
            committed(&ui);
        }
    });

    ui.on_routing_rule_edited({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move |i, row| {
            let ui = ui_weak.unwrap();
            {
                let mut st = app.lock().unwrap();
                let p = active_profile(&mut st.store.settings.routing);
                let Some(rule) = p.rules.get_mut(i as usize) else { return };
                *rule = rule_of(&row);
            }
            // Update in place: rebuilding the model would steal the text focus.
            let model = ui.get_routing_rules();
            if model.row_data(i as usize).as_ref() != Some(&row) {
                model.set_row_data(i as usize, row);
            }
            committed(&ui);
        }
    });

    let edit_rules = |ui: &AppWindow, app: &Shared, f: &dyn Fn(&mut Vec<RouteRule>)| {
        let mut st = app.lock().unwrap();
        let p = active_profile(&mut st.store.settings.routing);
        f(&mut p.rules);
        let rules: Vec<RuleRow> = p.rules.iter().map(rule_row).collect();
        ui.set_routing_rules(ModelRc::new(VecModel::from(rules)));
    };

    ui.on_routing_add_rule({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move || {
            let ui = ui_weak.unwrap();
            edit_rules(&ui, &app, &|rules| rules.push(RouteRule::default()));
            committed(&ui);
        }
    });
    ui.on_routing_rule_removed({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move |i| {
            let ui = ui_weak.unwrap();
            edit_rules(&ui, &app, &|rules| {
                if (i as usize) < rules.len() {
                    rules.remove(i as usize);
                }
            });
            committed(&ui);
        }
    });
    ui.on_routing_rule_moved({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move |i, delta| {
            let ui = ui_weak.unwrap();
            edit_rules(&ui, &app, &|rules| {
                let j = i + delta;
                if i >= 0 && j >= 0 && (i as usize) < rules.len() && (j as usize) < rules.len() {
                    rules.swap(i as usize, j as usize);
                }
            });
            committed(&ui);
        }
    });

    ui.on_routing_add_profile({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move || {
            let ui = ui_weak.unwrap();
            {
                let mut st = app.lock().unwrap();
                let r = &mut st.store.settings.routing;
                let n = r.profiles.len() + 1;
                r.profiles.push(RouteProfile { name: format!("Профиль {n}"), ..Default::default() });
                r.active_profile = r.profiles.len() - 1;
                load_profile(&ui, r);
            }
            committed(&ui);
        }
    });
    ui.on_routing_remove_profile({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move || {
            let ui = ui_weak.unwrap();
            {
                let mut st = app.lock().unwrap();
                let r = &mut st.store.settings.routing;
                if r.profiles.len() > 1 {
                    let i = r.active_profile.min(r.profiles.len() - 1);
                    r.profiles.remove(i);
                    r.active_profile = i.saturating_sub(1);
                }
                load_profile(&ui, r);
            }
            committed(&ui);
        }
    });
    ui.on_routing_select_profile({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move |i| {
            let ui = ui_weak.unwrap();
            {
                let mut st = app.lock().unwrap();
                let r = &mut st.store.settings.routing;
                r.active_profile = (i.max(0) as usize).min(r.profiles.len().saturating_sub(1));
                load_profile(&ui, r);
            }
            committed(&ui);
        }
    });

    ui.on_routing_update_geo({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || update_geo(&ui_weak.unwrap(), &app, true)
    });
    ui.on_routing_app_added({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move |name| {
            let ui = ui_weak.unwrap();
            let name = name.trim().to_string();
            {
                let mut st = app.lock().unwrap();
                let apps = &mut st.store.settings.routing.apps.apps;
                if name.is_empty() || apps.iter().any(|a| a.eq_ignore_ascii_case(&name)) {
                    return;
                }
                apps.push(name);
                load_apps(&ui, &st.store.settings.routing);
            }
            committed(&ui);
        }
    });
    ui.on_routing_app_removed({
        let (ui_weak, app, committed) = (ui.as_weak(), app.clone(), committed.clone());
        move |i| {
            let ui = ui_weak.unwrap();
            {
                let mut st = app.lock().unwrap();
                let apps = &mut st.store.settings.routing.apps.apps;
                if (i as usize) < apps.len() {
                    apps.remove(i as usize);
                }
                load_apps(&ui, &st.store.settings.routing);
            }
            committed(&ui);
        }
    });
    ui.on_routing_pick_running({
        let ui_weak = ui.as_weak();
        move || {
            let ui_weak = ui_weak.clone();
            std::thread::spawn(move || {
                let names = running_apps();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    let names: Vec<slint::SharedString> = names.into_iter().map(Into::into).collect();
                    ui.set_routing_running(ModelRc::new(VecModel::from(names)));
                });
            });
        }
    });
    ui.on_buy(|| crate::open_url(crate::BUY_URL));

    // Databases: now (if stale) and then every 6 hours.
    update_geo(ui, app, false);
    let timer = Box::leak(Box::new(slint::Timer::default()));
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(6 * 3600), move || {
        if let Some(ui) = ui_weak.upgrade() {
            update_geo(&ui, &app, false);
        }
    });
}

fn update_geo(ui: &AppWindow, app: &Shared, force: bool) {
    if ui.get_routing_geo_busy() {
        return;
    }
    let geo = geo_dir(app);
    ui.set_routing_geo_busy(true);
    if force {
        ui.set_routing_geo_status("Загрузка…".into());
    } else {
        ui.set_routing_geo_status(geo_status(&geo, None).into());
    }
    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        let bundled = connection::find_xray().ok().and_then(|(_, a)| a);
        let result = geo.update(bundled.as_deref(), force);
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            ui.set_routing_geo_busy(false);
            ui.set_routing_geo_status(geo_status(&geo, result.err().map(|e| format!("{e:#}"))).into());
        });
    });
}

fn geo_status(geo: &GeoDir, error: Option<String>) -> String {
    let when = geo
        .updated()
        .map(|t| {
            let t: chrono::DateTime<chrono::Local> = t.into();
            format!("Обновлено {}", t.format("%d.%m.%Y %H:%M"))
        })
        .unwrap_or_else(|| "Ещё не загружены: используются встроенные списки".into());
    match error {
        Some(e) => format!("{when}. Ошибка обновления: {}", crate::first_line(&e)),
        None => format!("{when}. Обновляются сами раз в сутки."),
    }
}

/// Names of running programs for the picker: app bundle names on macOS,
/// executable names elsewhere; system processes left out.
fn running_apps() -> Vec<String> {
    let mut names = std::collections::BTreeMap::new();
    let mut add = |n: &str| {
        let n = n.trim();
        if !n.is_empty() && !n.eq_ignore_ascii_case("duoray") && !n.eq_ignore_ascii_case("duoray.exe") {
            names.entry(n.to_lowercase()).or_insert_with(|| n.to_string());
        }
    };
    #[cfg(windows)]
    {
        const SYSTEM: &[&str] = &[
            "system", "system idle process", "registry", "smss.exe", "csrss.exe", "wininit.exe", "services.exe",
            "lsass.exe", "winlogon.exe", "svchost.exe", "dwm.exe", "fontdrvhost.exe", "conhost.exe", "sihost.exe",
            "runtimebroker.exe", "ctfmon.exe", "taskhostw.exe", "dllhost.exe", "wmiprvse.exe", "searchhost.exe",
            "searchindexer.exe", "spoolsv.exe", "audiodg.exe", "memory compression", "secure system",
            "duoray-helper.exe", "xray.exe",
        ];
        let mut cmd = std::process::Command::new("tasklist");
        cmd.args(["/fo", "csv", "/nh"]);
        if let Ok(out) = connection::hidden(&mut cmd).output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let name = line.split("\",\"").next().unwrap_or_default().trim_matches('"');
                if !SYSTEM.contains(&name.to_lowercase().as_str()) {
                    add(name);
                }
            }
        }
    }
    // Executable names as the tunnel sees them (/proc/<pid>/exe), plus the
    // argv[0] name of apps run by a shared runtime (Electron, Python, Java):
    // `ps` truncates names to 15 characters and shows neither reliably.
    #[cfg(target_os = "linux")]
    {
        const SYSTEM: &[&str] = &[
            "systemd", "dbus-daemon", "dbus-broker", "dbus-broker-launch", "pipewire", "pipewire-pulse",
            "wireplumber", "xdg-desktop-portal", "xdg-document-portal", "xdg-permission-store", "gvfsd",
            "at-spi-bus-launcher", "at-spi2-registryd", "bash", "sh", "zsh", "fish", "dash", "sudo", "su",
            "login", "ps", "xray", "duoray-helper", "xwayland", "pulseaudio", "gnome-keyring-daemon", "ssh-agent",
            "gpg-agent", "dconf-service", "flatpak-portal", "flatpak-session-helper", "bwrap",
        ];
        const SYSTEM_DIRS: &[&str] = &["/usr/lib/systemd/", "/lib/systemd/", "/usr/libexec/", "/usr/lib/polkit"];
        const RUNTIMES: &[&str] = &["electron", "python", "java", "node", "mono", "wine", "dotnet"];
        // SAFETY: plain syscall.
        let uid = unsafe { libc::getuid() };
        let entries = std::fs::read_dir("/proc").into_iter().flatten().flatten();
        for entry in entries {
            use std::os::unix::fs::MetadataExt;
            if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())
                || entry.metadata().ok().map(|m| m.uid()) != Some(uid)
            {
                continue;
            }
            let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else { continue };
            let path = exe.to_string_lossy();
            if SYSTEM_DIRS.iter().any(|d| path.starts_with(d)) {
                continue;
            }
            let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let lower = name.to_lowercase();
            if RUNTIMES.iter().any(|r| lower.starts_with(r)) {
                let argv0 = std::fs::read(entry.path().join("cmdline")).ok().and_then(|c| {
                    let first = c.split(|b| *b == 0).next()?.to_vec();
                    let first = String::from_utf8_lossy(&first).into_owned();
                    Some(first.rsplit('/').next().unwrap_or(&first).to_string())
                });
                if let Some(a) = argv0.filter(|a| !a.is_empty() && !a.starts_with('-') && *a != name) {
                    add(&a);
                    continue;
                }
            }
            let family = ["xdg-desktop-portal", "xdg-document", "gvfsd", "at-spi", "dbus-"];
            if !SYSTEM.contains(&lower.as_str()) && !family.iter().any(|f| lower.starts_with(f)) {
                add(&name);
            }
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        const SYSTEM_DIRS: &[&str] = &["/System/", "/usr/libexec/", "/usr/sbin/", "/sbin/", "/Library/Apple/", "/usr/lib/"];
        if let Ok(out) = std::process::Command::new("ps").args(["-axo", "comm="]).output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let path = line.trim();
                // Login shells show up as "-zsh"; system daemons and agents live in system dirs.
                if path.starts_with('-') || SYSTEM_DIRS.iter().any(|d| path.starts_with(d)) {
                    continue;
                }
                if let Some(i) = path.find(".app/") {
                    // The outermost bundle: helpers inside Chrome.app count as Chrome.
                    let bundle = &path[..i];
                    add(bundle.rsplit('/').next().unwrap_or(bundle));
                } else {
                    add(path.rsplit('/').next().unwrap_or(path));
                }
            }
        }
    }
    names.into_values().collect()
}

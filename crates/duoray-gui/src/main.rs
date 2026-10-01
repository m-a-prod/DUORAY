// Release builds on Windows have no console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duoray_core::device::Device;
use duoray_core::identity::{self, HappSpoof};
use duoray_core::server::display_name;
use duoray_core::store::{self, Store, Subscription};
use duoray_core::subscription;
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

slint::include_modules!();

mod connection;
mod helper;
mod ping;
#[cfg(windows)]
mod windows_helper;

use connection::{ConnectError, Connection};

mod flags {
    include!(concat!(env!("OUT_DIR"), "/flags.rs"));
}

thread_local! {
    static FLAG_CACHE: std::cell::RefCell<std::collections::HashMap<String, Option<slint::Image>>> =
        Default::default();
}

/// Decoded, cached flag image for an ISO code ("SE"); `None` if we have none.
fn flag_image(code: &str) -> Option<slint::Image> {
    FLAG_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(code.to_string())
            .or_insert_with(|| flags::flag_png(code).and_then(decode_png))
            .clone()
    })
}

fn decode_png(bytes: &[u8]) -> Option<slint::Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let px = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.as_chunks::<3>().0.iter().flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.as_chunks::<2>().0.iter().flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, info.width, info.height);
    Some(slint::Image::from_rgba8(buffer))
}

struct App {
    store: Store,
    path: PathBuf,
    /// Subscription ids currently being fetched.
    refreshing: HashSet<String>,
    /// When each subscription was last fetched (successfully or not), for auto-update pacing.
    last_attempt: std::collections::HashMap<String, std::time::Instant>,
    status: String,
    conn: Option<Connection>,
    conn_state: ConnState,
    /// Incremented per connect attempt; stale failure reports are ignored.
    conn_gen: u64,
    /// Connect again once the helper is installed.
    connect_after_install: bool,
    /// Latest ping per server key: (label, level) as shown in the row.
    /// Latest ping per server key: (label, level, sort key in ms).
    ping_results: std::collections::HashMap<String, (String, i32, u128)>,
    /// Show servers ordered by ping instead of the panel's order.
    sort_by_ping: bool,
    /// Latency of the active connection, re-measured every 5 s: (label, level).
    live_ping: Option<(String, i32)>,
    /// A live measurement is in flight (never stack them).
    live_ping_busy: bool,
    /// (done, total) while a ping run is active.
    ping_progress: Option<(usize, usize)>,
    /// Bumped per transient status message, so an old timer never clears a newer one.
    status_gen: u64,
}

enum ConnState {
    Idle,
    Connecting { name: String },
    /// `key` identifies the server (see `server_key`).
    Connected { name: String, tun: String, key: String },
    /// Tearing down the old server before connecting to the newly selected one.
    Switching { name: String },
    Disconnecting,
    Failed(String),
}

type Shared = Arc<Mutex<App>>;

fn main() -> anyhow::Result<()> {
    // Wayland app_id / X11 WM_CLASS: ties the window to duoray.desktop and its icon.
    let _ = slint::set_xdg_app_id("duoray");
    let path = Store::default_path()?;
    let (store, status) = match Store::load(&path) {
        Ok(s) => (s, String::new()),
        Err(e) => (Store::default(), format!("Не удалось прочитать данные: {e:#}")),
    };
    let app: Shared = Arc::new(Mutex::new(App {
        store,
        path,
        refreshing: HashSet::new(),
        last_attempt: Default::default(),
        status,
        conn: None,
        conn_state: ConnState::Idle,
        conn_gen: 0,
        connect_after_install: false,
        ping_results: Default::default(),
        sort_by_ping: false,
        live_ping: None,
        live_ping_busy: false,
        ping_progress: None,
        status_gen: 0,
    }));
    let device = Arc::new(Device::detect());

    let ui = AppWindow::new()?;

    ui.set_font_choices(ModelRc::new(VecModel::from(
        std::iter::once(SYSTEM_FONT).chain(FONTS.iter().copied()).map(slint::SharedString::from).collect::<Vec<_>>(),
    )));
    {
        let st = app.lock().unwrap();
        apply_appearance(&ui, &st.store.settings);
        if !restore_selection(&ui, &st) && !st.store.subscriptions.is_empty() {
            ui.set_current_sub(0);
        }
        render(&ui, &st);
    }

    ui.on_select_sub({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |i| {
            let ui = ui_weak.unwrap();
            ui.set_current_sub(i);
            ui.set_current_server(-1);
            ui.set_confirm_remove_index(-1);
            let st = app.lock().unwrap();
            restore_selection_in_current(&ui, &st);
            render(&ui, &st);
        }
    });

    ui.on_select_server({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |i| {
            let ui = ui_weak.unwrap();
            ui.set_current_server(i);
            {
                let mut st = app.lock().unwrap();
                st.store.settings.last_server = server_key(&ui, &st);
                save(&mut st);
                render(&ui, &st);
            }
            follow_selection(&ui, &app);
        }
    });

    ui.on_toggle_connection({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || toggle_connection(&ui_weak.unwrap(), &app)
    });

    ui.on_install_helper({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            ui.set_helper_busy(true);
            ui.set_helper_error("".into());
            let (ui_weak, app) = (ui.as_weak(), app.clone());
            std::thread::spawn(move || {
                let result = helper::install_and_wait();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_helper_busy(false);
                    match result {
                        Ok(()) => {
                            ui.set_helper_dialog(false);
                            let retry = std::mem::take(&mut app.lock().unwrap().connect_after_install);
                            if retry {
                                start_connect(&ui, &app);
                            }
                        }
                        Err(e) => ui.set_helper_error(format!("{e:#}").into()),
                    }
                });
            });
        }
    });

    ui.on_add_subscription({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move |url, name| {
            let ui = ui_weak.unwrap();
            match add_input(&ui, &app, &device, &url, &name) {
                Ok(()) => ui.set_adding(false),
                Err(e) => ui.set_add_error(humanize(&format!("{e:#}")).into()),
            }
        }
    });

    ui.on_refresh_current({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let ui = ui_weak.unwrap();
            // Separate statement: the guard must be released before refresh() locks again.
            let id = current(&ui, &app.lock().unwrap()).map(|s| s.id.clone());
            if let Some(id) = id {
                refresh(&ui, &app, &device, id);
            }
        }
    });

    ui.on_remove_sub({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |i| {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let Some(id) = usize::try_from(i).ok().and_then(|i| st.store.subscriptions.get(i)).map(|s| s.id.clone())
            else {
                return;
            };
            let was_current = ui.get_current_sub() == i;
            st.store.remove(&id);
            save(&mut st);
            let n = st.store.subscriptions.len() as i32;
            let cur = ui.get_current_sub();
            // Keep the same subscription selected if one above it was removed.
            ui.set_current_sub(if was_current { cur.min(n - 1) } else if cur > i { cur - 1 } else { cur });
            if was_current {
                ui.set_current_server(-1);
                restore_selection_in_current(&ui, &st);
            }
            render(&ui, &st);
        }
    });

    // ── Settings page ───────────────────────────────────────────
    ui.on_open_settings({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let ui = ui_weak.unwrap();
            ui.set_duoray_ua(identity::DUORAY_USER_AGENT.into());
            ui.set_duoray_hwid(device.hwid.clone().into());
            ui.set_settings_helper_status(helper::describe().into());
            let st = app.lock().unwrap();
            load_happ(&ui, &st.store.settings.happ);
            load_ping(&ui, &st.store.settings.ping);
            ui.set_send_device_info(st.store.settings.send_device_info);
            ui.set_text_scale_choice(format!("{}%", st.store.settings.text_scale).into());
            let theme = THEMES.iter().find(|t| t.0 == st.store.settings.theme).map_or(THEMES[0].1, |t| t.1);
            ui.set_theme_choice(theme.into());
            ui.set_font_choice(
                if st.store.settings.font.is_empty() { SYSTEM_FONT.to_string() } else { st.store.settings.font.clone() }.into(),
            );
        }
    });
    // Settings save themselves: every edit restarts a short timer, so typing
    // in a field does not rewrite the store on each key press.
    let settings_debounce = std::rc::Rc::new(slint::Timer::default());
    ui.on_settings_changed({
        let (ui_weak, app, timer) = (ui.as_weak(), app.clone(), settings_debounce.clone());
        move || {
            let (ui_weak, app) = (ui_weak.clone(), app.clone());
            timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(500), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    save_settings_from_ui(&ui, &app);
                }
            });
        }
    });
    ui.on_randomize_ids({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            let mut h = read_happ(&ui);
            h.randomize_ids();
            load_happ(&ui, &h);
            ui.invoke_settings_changed();
        }
    });
    ui.on_randomize_device({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            let mut h = read_happ(&ui);
            h.randomize_device();
            load_happ(&ui, &h);
            ui.invoke_settings_changed();
        }
    });
    for uninstall in [false, true] {
        let ui_weak = ui.as_weak();
        let handler = move || {
            let ui = ui_weak.unwrap();
            ui.set_settings_helper_busy(true);
            ui.set_settings_helper_status("Нужен пароль администратора…".into());
            let ui_weak = ui.as_weak();
            std::thread::spawn(move || {
                let result = if uninstall { helper::uninstall() } else { helper::install_and_wait() };
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_settings_helper_busy(false);
                    let status = helper::describe();
                    ui.set_settings_helper_status(
                        match result {
                            Ok(()) => status,
                            Err(e) => format!("{e:#}. {status}"),
                        }
                        .into(),
                    );
                });
            });
        };
        if uninstall {
            ui.on_uninstall_helper(handler);
        } else {
            ui.on_reinstall_helper(handler);
        }
    }
    ui.on_reset_happ({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            load_happ(&ui, &HappSpoof::default());
            ui.invoke_settings_changed();
        }
    });

    ui.on_open_web_page({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let url = current(&ui, &app.lock().unwrap()).and_then(|s| s.info.web_page_url.clone());
            if let Some(url) = url {
                open_url(&url);
            }
        }
    });
    ui.on_open_support({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let url = current(&ui, &app.lock().unwrap()).and_then(|s| s.info.support_url.clone());
            if let Some(url) = url {
                open_url(&url);
            }
        }
    });
    ui.on_toggle_announce({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let Some(i) = usize::try_from(ui.get_current_sub()).ok().filter(|&i| i < st.store.subscriptions.len()) else {
                return;
            };
            let sub = &mut st.store.subscriptions[i];
            sub.announce_hidden = !sub.announce_hidden;
            save(&mut st);
            render(&ui, &st);
        }
    });
    ui.on_toggle_sort({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            st.sort_by_ping = !st.sort_by_ping;
            render(&ui, &st);
        }
    });
    ui.on_ping_current({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || start_ping(&ui_weak.unwrap(), &app)
    });
    ui.on_add_from_clipboard({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let ui = ui_weak.unwrap();
            let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
            let url = text.trim().to_string();
            match add_input(&ui, &app, &device, &url, "") {
                Ok(()) => {}
                // Not a usable link: open the manual dialog and say why.
                Err(e) => {
                    let why = if url.is_empty() {
                        "В буфере обмена нет текста. Вставьте ссылку вручную.".to_string()
                    } else {
                        format!("В буфере не ссылка на подписку: {}", humanize(&format!("{e:#}")))
                    };
                    ui.set_add_error(why.into());
                    ui.set_adding(true);
                }
            }
        }
    });
    ui.on_copy_sub_link({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            // Hand-added groups have no URL: copy their share links instead.
            let Some(text) = current(&ui, &st).map(|s| {
                if s.is_manual() {
                    s.servers
                        .iter()
                        .filter_map(|x| match &x.source {
                            duoray_core::server::Source::Link(p) => Some(p.link.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    s.url.clone()
                }
            }) else {
                return;
            };
            let msg = match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                Ok(()) => "Скопировано в буфер обмена".into(),
                Err(e) => format!("Не удалось скопировать: {e}"),
            };
            flash_status(&ui, &app, &mut st, msg);
            render(&ui, &st);
        }
    });

    // While connected: latency through the live tunnel every 5 s.
    let live_ping_timer = slint::Timer::default();
    live_ping_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(5), {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut st = app.lock().unwrap();
            measure_live_ping(&ui, &app, &mut st);
        }
    });

    // Auto-update: each subscription at the panel's interval. After a failure
    // the next try waits at least 10 minutes.
    let auto_update = slint::Timer::default();
    auto_update.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(60), {
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let due: Vec<String> = {
                let st = app.lock().unwrap();
                let now = store::now();
                st.store
                    .subscriptions
                    .iter()
                    .filter(|s| !s.is_manual())
                    .filter(|s| {
                        let interval = u64::from(update_interval_hours(s)) * 3600;
                        let stale = s.updated_at.is_none_or(|t| now.saturating_sub(t) >= interval);
                        let backoff = st
                            .last_attempt
                            .get(&s.id)
                            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(600));
                        stale && !backoff
                    })
                    .map(|s| s.id.clone())
                    .collect()
            };
            for id in due {
                refresh(&ui, &app, &device, id);
            }
        }
    });

    // Refresh everything on start; the last good list stays visible meanwhile.
    let ids: Vec<String> = app.lock().unwrap().store.subscriptions.iter().map(|s| s.id.clone()).collect();
    for id in ids {
        refresh(&ui, &app, &device, id);
    }

    // Dev hook: DUORAY_SNAPSHOT=/path/prefix saves PNGs of the servers and settings views.
    if let Some(prefix) = std::env::var_os("DUORAY_SNAPSHOT") {
        let prefix = PathBuf::from(prefix);
        let ui_weak = ui.as_weak();
        {
            let ui_weak = ui_weak.clone();
            slint::Timer::single_shot(std::time::Duration::from_secs(6), move || {
                ui_weak.unwrap().invoke_ping_current();
            });
        }
        slint::Timer::single_shot(std::time::Duration::from_secs(16), move || {
            let ui = ui_weak.unwrap();
            // Exercise the refresh button (it once deadlocked).
            ui.invoke_refresh_current();
            // Show the delete confirmation and the ping sort so both are visible in the shot.
            ui.set_confirm_remove_index(0);
            ui.invoke_toggle_sort();
            snapshot(ui.window(), &prefix.with_extension("main.png"));
            ui.set_confirm_remove_index(-1);
            ui.invoke_open_settings();
            ui.set_settings_open(true);
            let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
            slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                snapshot(ui_weak.unwrap().window(), &prefix.with_extension("settings.png"));
                let _ = slint::quit_event_loop();
            });
        });
    }

    ui.run()?;

    // Window closed: bring the network back before exiting.
    let conn = app.lock().unwrap().conn.take();
    if let Some(conn) = conn {
        conn.disconnect();
    }
    Ok(())
}

/// Indices into `sub.servers` in the order the list shows them. Row numbers in
/// the UI always refer to this order.
fn display_order(st: &App, sub: &Subscription) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sub.servers.len()).collect();
    if st.sort_by_ping {
        // Stable: equal pings keep the panel's order. Unmeasured and failed go last.
        order.sort_by_key(|&i| st.ping_results.get(&key_of(sub, &sub.servers[i])).map_or(u128::MAX, |p| p.2));
    }
    order
}

fn selected_server(ui: &AppWindow, st: &App) -> Option<duoray_core::server::Server> {
    let sub = current(ui, st)?;
    let row = usize::try_from(ui.get_current_server()).ok()?;
    let i = *display_order(st, sub).get(row)?;
    sub.servers.get(i).cloned()
}

/// Row of the server with `key` in `sub`, in display order.
fn row_of(st: &App, sub: &Subscription, key: &str) -> Option<usize> {
    display_order(st, sub).iter().position(|&i| key_of(sub, &sub.servers[i]) == key)
}

/// Stable identity of the selected server; survives list refreshes and reordering.
fn server_key(ui: &AppWindow, st: &App) -> Option<String> {
    let sub = current(ui, st)?;
    let s = selected_server(ui, st)?;
    Some(key_of(sub, &s))
}

fn key_of(sub: &Subscription, s: &duoray_core::server::Server) -> String {
    format!("{}\u{1f}{}\u{1f}{}:{}", sub.id, s.name, s.address, s.port)
}

/// Selects the remembered server anywhere; `false` if there is none.
fn restore_selection(ui: &AppWindow, st: &App) -> bool {
    let Some(want) = &st.store.settings.last_server else { return false };
    for (si, sub) in st.store.subscriptions.iter().enumerate() {
        if let Some(row) = row_of(st, sub, want) {
            ui.set_current_sub(si as i32);
            ui.set_current_server(row as i32);
            return true;
        }
    }
    false
}

/// Selects the remembered server if it is in the currently shown subscription.
fn restore_selection_in_current(ui: &AppWindow, st: &App) {
    let (Some(want), Some(sub)) = (&st.store.settings.last_server, current(ui, st)) else { return };
    if let Some(row) = row_of(st, sub, want) {
        ui.set_current_server(row as i32);
    }
}

/// While connected, picking another server switches the connection to it.
fn follow_selection(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    let Some(wanted) = server_key(ui, &st) else { return };
    let ConnState::Connected { key, .. } = &st.conn_state else { return };
    if *key == wanted {
        return;
    }
    let name = selected_server(ui, &st).map(|s| display_name(&s.name).1).unwrap_or_default();
    let conn = st.conn.take();
    // Invalidate failure reports of the connection being replaced.
    st.conn_gen += 1;
    st.conn_state = ConnState::Switching { name };
    render(ui, &st);
    drop(st);

    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        if let Some(c) = conn {
            c.disconnect();
        }
        let _ = ui_weak.upgrade_in_event_loop(move |ui| start_connect(&ui, &app));
    });
}

fn toggle_connection(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    match st.conn_state {
        ConnState::Connected { .. } => {
            let conn = st.conn.take();
            st.conn_state = ConnState::Disconnecting;
            render(ui, &st);
            let (ui_weak, app) = (ui.as_weak(), app.clone());
            std::thread::spawn(move || {
                if let Some(c) = conn {
                    c.disconnect();
                }
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    let mut st = app.lock().unwrap();
                    st.conn_state = ConnState::Idle;
                    render(&ui, &st);
                });
            });
        }
        ConnState::Idle | ConnState::Failed(_) => {
            drop(st);
            start_connect(ui, app);
        }
        _ => {}
    }
}

fn start_connect(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    let (Some(server), Some(key)) = (selected_server(ui, &st), server_key(ui, &st)) else {
        st.conn_state = ConnState::Idle;
        render(ui, &st);
        return;
    };
    let name = display_name(&server.name).1;
    st.conn_gen += 1;
    let generation = st.conn_gen;
    st.conn_state = ConnState::Connecting { name: name.clone() };
    let run_dir = st.path.parent().map(|p| p.join("run")).unwrap_or_else(|| PathBuf::from("run"));
    render(ui, &st);
    drop(st);

    let on_failure = {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |msg: String| {
            let app = app.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| connection_failed(&ui, &app, generation, msg));
        }
    };
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let result = connection::connect(&server, &run_dir, on_failure);
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            let still_wanted = st.conn_gen == generation && matches!(st.conn_state, ConnState::Connecting { .. });
            match result {
                Ok(conn) if still_wanted => {
                    st.conn_state = ConnState::Connected { name, tun: conn.tun.clone(), key };
                    st.conn = Some(conn);
                    st.live_ping = None;
                    // First reading right away, then every 5 s from the timer.
                    measure_live_ping(&ui, &app, &mut st);
                }
                Ok(conn) => {
                    std::thread::spawn(move || conn.disconnect());
                }
                Err(ConnectError::HelperMissing) => {
                    st.conn_state = ConnState::Idle;
                    st.connect_after_install = true;
                    ui.set_helper_error("".into());
                    ui.set_helper_dialog_text(
                        "Для VPN-туннеля (TUN) DUORAY нужен небольшой системный помощник. Он устанавливается один раз: \
                         система попросит пароль администратора. Дальше подключение работает без пароля."
                            .into(),
                    );
                    ui.set_helper_dialog(true);
                }
                Err(ConnectError::HelperOutdated(v)) => {
                    st.conn_state = ConnState::Idle;
                    st.connect_after_install = true;
                    ui.set_helper_error("".into());
                    ui.set_helper_dialog_text(
                        format!("Установлена старая версия помощника ({v}). Обновите её, понадобится пароль администратора.")
                            .into(),
                    );
                    ui.set_helper_dialog(true);
                }
                Err(ConnectError::Other(e)) => st.conn_state = ConnState::Failed(humanize(&format!("{e:#}"))),
            }
            render(&ui, &st);
            drop(st);
            // The user may have picked another server while we were connecting.
            follow_selection(&ui, &app);
        });
    });
}

fn connection_failed(ui: &AppWindow, app: &Shared, generation: u64, msg: String) {
    let mut st = app.lock().unwrap();
    if st.conn_gen != generation || !matches!(st.conn_state, ConnState::Connected { .. }) {
        return;
    }
    let conn = st.conn.take();
    st.conn_state = ConnState::Failed(if msg.is_empty() { "Помощник отключился".into() } else { msg });
    render(ui, &st);
    if let Some(c) = conn {
        std::thread::spawn(move || c.disconnect());
    }
}

fn snapshot(window: &slint::Window, path: &std::path::Path) {
    let result = window.take_snapshot().map_err(|e| e.to_string()).and_then(|img| {
        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width(), img.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(img.as_bytes()).map_err(|e| e.to_string())
    });
    match result {
        Ok(()) => eprintln!("snapshot saved: {}", path.display()),
        Err(e) => eprintln!("snapshot failed: {e}"),
    }
}

fn load_happ(w: &AppWindow, h: &HappSpoof) {
    w.set_happ_enabled(h.enabled);
    w.set_app_version(h.app_version.clone().into());
    w.set_os(h.os.clone().into());
    w.set_os_version(h.os_version.clone().into());
    w.set_model(h.model.clone().into());
    w.set_locale(h.locale.clone().into());
    w.set_user_id(h.user_id.clone().into());
    w.set_hwid(h.hwid.clone().into());
}

fn read_happ(w: &AppWindow) -> HappSpoof {
    HappSpoof {
        enabled: w.get_happ_enabled(),
        app_version: w.get_app_version().trim().to_string(),
        os: w.get_os().trim().to_string(),
        os_version: w.get_os_version().trim().to_string(),
        model: w.get_model().trim().to_string(),
        locale: w.get_locale().trim().to_string(),
        user_id: w.get_user_id().trim().to_string(),
        hwid: w.get_hwid().trim().to_string(),
    }
}

fn current<'a>(ui: &AppWindow, st: &'a App) -> Option<&'a Subscription> {
    usize::try_from(ui.get_current_sub()).ok().and_then(|i| st.store.subscriptions.get(i))
}

fn save(st: &mut App) {
    if let Err(e) = st.store.save(&st.path) {
        st.status = format!("Не удалось сохранить: {e:#}");
    }
}

fn refresh(ui: &AppWindow, app: &Shared, device: &Arc<Device>, id: String) {
    let (url, fallback, headers) = {
        let mut st = app.lock().unwrap();
        if !st.refreshing.insert(id.clone()) {
            return;
        }
        st.last_attempt.insert(id.clone(), std::time::Instant::now());
        let Some(sub) = st.store.get(&id) else { return };
        if sub.is_manual() {
            st.refreshing.remove(&id);
            return;
        }
        let (url, fallback) = (sub.url.clone(), sub.info.fallback_url.clone());
        (url, fallback, identity::request_headers(&st.store.settings.happ, device, st.store.settings.send_device_info))
    };
    render(ui, &app.lock().unwrap());

    let (ui_weak, app): (Weak<AppWindow>, Shared) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let result = subscription::fetch(&url, fallback.as_deref(), &headers).map_err(|e| humanize(&format!("{e:#}")));
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.refreshing.remove(&id);
            st.store.apply(&id, result);
            save(&mut st);
            // The list may have been reordered: find the selected server again.
            if current(&ui, &st).is_some_and(|s| s.id == id) {
                ui.set_current_server(-1);
                restore_selection_in_current(&ui, &st);
            }
            render(&ui, &st);
        });
    });
}

fn render(ui: &AppWindow, st: &App) {
    let subs: Vec<SubRow> = st
        .store
        .subscriptions
        .iter()
        .map(|s| SubRow {
            name: s.display_name().into(),
            meta: sub_meta(s).into(),
            usage: usage(s),
            error: s.last_error.as_deref().map(first_line).unwrap_or_default().into(),
        })
        .collect();
    ui.set_subs(ModelRc::new(VecModel::from(subs)));

    let busy_count = st.refreshing.len();
    ui.set_status(
        if let Some((done, total)) = st.ping_progress {
            format!("Пинг: {done} из {total}")
        } else if busy_count > 0 {
            format!("Обновление подписок… ({busy_count})")
        } else {
            st.status.clone()
        }
        .into(),
    );

    let (state, title, detail) = match &st.conn_state {
        ConnState::Idle if selected_server(ui, st).is_none() => (
            "idle",
            "Сервер не выбран".to_string(),
            if st.store.subscriptions.is_empty() {
                "Добавьте подписку, затем выберите сервер".to_string()
            } else {
                "Выберите сервер в списке справа".to_string()
            },
        ),
        ConnState::Idle => ("idle", "Не подключено".to_string(), String::new()),
        ConnState::Connecting { name, .. } => ("connecting", "Подключение…".into(), name.clone()),
        ConnState::Switching { name } => ("connecting", "Переключение…".into(), name.clone()),
        ConnState::Connected { name, tun, .. } => ("connected", "Подключено".into(), format!("{name} · {tun}")),
        ConnState::Disconnecting => ("disconnecting", "Отключение…".into(), String::new()),
        ConnState::Failed(e) => ("failed", "Ошибка подключения".into(), first_line(e)),
    };
    ui.set_conn_state(state.into());
    ui.set_conn_title(title.into());
    ui.set_conn_detail(detail.into());
    let (live_text, live_level) = match (&st.conn_state, &st.live_ping) {
        (ConnState::Connected { .. }, Some(p)) => p.clone(),
        (ConnState::Connected { .. }, None) => ("…".to_string(), 5),
        _ => (String::new(), 0),
    };
    ui.set_conn_ping_text(live_text.into());
    ui.set_conn_ping_level(live_level);
    ui.set_can_connect(selected_server(ui, st).is_some());

    let Some(sub) = current(ui, st) else {
        ui.set_servers(ModelRc::new(VecModel::from(Vec::<ServerRow>::new())));
        return;
    };
    ui.set_header_title(sub.display_name().into());
    ui.set_header_meta(header_meta(sub).into());
    let (usage_text, usage_fraction) = usage_line(sub);
    ui.set_usage_text(usage_text.into());
    ui.set_usage_fraction(usage_fraction);
    ui.set_has_web_page(sub.info.web_page_url.is_some());
    ui.set_has_support(sub.info.support_url.is_some());
    ui.set_show_usage_row(!sub.is_manual());
    ui.set_announce(sub.info.announce.clone().unwrap_or_default().into());
    ui.set_announce_hidden(sub.announce_hidden);
    ui.set_sub_error(sub.last_error.clone().unwrap_or_default().into());
    ui.set_busy(st.refreshing.contains(&sub.id));

    let order = display_order(st, sub);
    ui.set_sort_by_ping(st.sort_by_ping);
    ui.set_has_ping_results(sub.servers.iter().any(|s| st.ping_results.contains_key(&key_of(sub, s))));
    // Rows move when sorted by ping: keep the highlight on the selected server.
    if let Some(row) = st.store.settings.last_server.as_deref().and_then(|k| row_of(st, sub, k)) {
        ui.set_current_server(row as i32);
    }
    let rows: Vec<ServerRow> = order
        .iter()
        .map(|&i| &sub.servers[i])
        .map(|s| {
            let (flag, name) = display_name(&s.name);
            let image = flag.as_deref().and_then(flag_image);
            let ping = st.ping_results.get(&key_of(sub, s)).cloned().unwrap_or_default();
            ServerRow {
                has_flag_image: image.is_some(),
                flag_image: image.unwrap_or_default(),
                flag: flag.unwrap_or_default().into(),
                name: name.into(),
                description: s.description.clone().unwrap_or_default().into(),
                protocol: s.protocol.clone().into(),
                transport: transport(&s.network, &s.security).into(),
                endpoint: format!("{}:{}", s.address, s.port).into(),
                proxies: s.proxies as i32,
                ping_text: ping.0.into(),
                ping_level: ping.1,
            }
        })
        .collect();
    ui.set_servers(ModelRc::new(VecModel::from(rows)));
}

fn transport(network: &str, security: &str) -> String {
    match security {
        "" | "none" => network.to_string(),
        s => format!("{network} · {s}"),
    }
}

fn usage(s: &Subscription) -> f32 {
    match s.info.total {
        Some(total) if total > 0 => {
            let used = s.info.download.unwrap_or(0) + s.info.upload.unwrap_or(0);
            used as f32 / total as f32
        }
        _ => -1.0,
    }
}

fn sub_meta(s: &Subscription) -> String {
    let mut parts = vec![];
    if s.updated_at.is_some() || !s.servers.is_empty() {
        parts.push(plural(s.servers.len(), "сервер", "сервера", "серверов"));
    } else {
        parts.push("не загружена".to_string());
    }
    let used = s.info.download.unwrap_or(0) + s.info.upload.unwrap_or(0);
    match s.info.total {
        Some(t) => parts.push(format!("{} из {}", bytes(used), bytes(t))),
        None if used > 0 => parts.push(format!("{} · безлимит", bytes(used))),
        None => {}
    }
    if let Some(e) = s.info.expire {
        parts.push(format!("до {}", date(e)));
    }
    parts.join(" · ")
}

/// "01.10.2026 01:29 · Автообновление — 1 ч · 56 серверов"
fn header_meta(s: &Subscription) -> String {
    if s.is_manual() {
        return format!("{} · добавлены вручную", plural(s.servers.len(), "сервер", "сервера", "серверов"));
    }
    let mut parts = vec![];
    match s.updated_at {
        Some(t) => parts.push(local_time(t)),
        None => parts.push("ещё не обновлялась".into()),
    }
    parts.push(format!("Автообновление — {} ч", update_interval_hours(s)));
    parts.push(plural(s.servers.len(), "сервер", "сервера", "серверов"));
    if s.skipped > 0 {
        parts.push(format!("пропущено {}", s.skipped));
    }
    parts.join(" · ")
}

fn local_time(unix: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(unix as i64, 0)
        .single()
        .map(|t| t.format("%d.%m.%Y %H:%M").to_string())
        .unwrap_or_else(|| date(unix))
}

/// The panel's `profile-update-interval`, or 12 h.
fn update_interval_hours(s: &Subscription) -> u32 {
    s.info.update_interval_hours.filter(|h| *h > 0).unwrap_or(12)
}

/// "86.0 ГБ / ∞" or "12.4 ГБ / 100.0 ГБ" and the used fraction (negative = unlimited).
fn usage_line(s: &Subscription) -> (String, f32) {
    let used = s.info.download.unwrap_or(0) + s.info.upload.unwrap_or(0);
    match s.info.total {
        Some(t) if t > 0 => (format!("{} / {}", bytes(used), bytes(t)), used as f32 / t as f32),
        _ => (format!("{} / ∞", bytes(used)), -1.0),
    }
}

/// Opens a web or Telegram link in the system handler.
fn open_url(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://") || url.starts_with("tg://")) {
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(windows)]
    let _ = connection::hidden(std::process::Command::new("cmd").args(["/C", "start", "", url])).spawn();
}

fn plural(n: usize, one: &str, few: &str, many: &str) -> String {
    let word = match (n % 10, n % 100) {
        (1, r) if r != 11 => one,
        (2..=4, r) if !(12..=14).contains(&r) => few,
        _ => many,
    };
    format!("{n} {word}")
}

fn bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{b} Б") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// Unix seconds -> DD.MM.YYYY (UTC), via Howard Hinnant's civil_from_days.
fn date(unix: u64) -> String {
    let z = (unix / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{d:02}.{m:02}.{y}")
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

/// Maps the common English errors from core to something readable.
fn humanize(e: &str) -> String {
    let map = [
        ("already added", "Эта подписка уже добавлена."),
        ("not a valid URL", "Это не похоже на ссылку."),
        ("must be http(s)", "Ссылка должна начинаться с http:// или https://."),
        ("HTML page", "Вместо подписки пришла веб-страница. Проверьте ссылку."),
        ("timed out", "Сервер подписки не ответил вовремя."),
        ("status: 404", "Подписка не найдена (404)."),
        ("status: 403", "Доступ к подписке запрещён (403). Возможно, поможет Happ Spoof в настройках."),
    ];
    for (needle, text) in map {
        if e.contains(needle) {
            return text.to_string();
        }
    }
    e.to_string()
}

const PING_THREADS: [u32; 5] = [1, 2, 4, 8, 16];

fn load_ping(ui: &AppWindow, p: &duoray_core::store::PingSettings) {
    use duoray_core::store::PingMode;
    ui.set_ping_mode_index(match p.mode {
        PingMode::HttpGet => 0,
        PingMode::HttpHead => 1,
        PingMode::Tcp => 2,
        PingMode::Icmp => 3,
    });
    let threads = PING_THREADS.iter().copied().filter(|t| *t <= p.threads).max().unwrap_or(1);
    ui.set_ping_threads(threads.to_string().into());
    ui.set_ping_url(p.url.clone().into());
}

fn read_ping(ui: &AppWindow, old: &duoray_core::store::PingSettings) -> duoray_core::store::PingSettings {
    use duoray_core::store::PingMode;
    let url = ui.get_ping_url().trim().to_string();
    duoray_core::store::PingSettings {
        mode: match ui.get_ping_mode_index() {
            1 => PingMode::HttpHead,
            2 => PingMode::Tcp,
            3 => PingMode::Icmp,
            _ => PingMode::HttpGet,
        },
        threads: ui.get_ping_threads().parse().unwrap_or(1),
        url: if url.starts_with("http://") || url.starts_with("https://") { url } else { old.url.clone() },
        timeout_ms: old.timeout_ms,
    }
}

fn save_settings_from_ui(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    st.store.settings.happ = read_happ(ui);
    st.store.settings.ping = read_ping(ui, &st.store.settings.ping);
    st.store.settings.send_device_info = ui.get_send_device_info();
    let theme = ui.get_theme_choice();
    st.store.settings.theme = THEMES.iter().find(|t| t.1 == theme.as_str()).map_or("dark", |t| t.0).to_string();
    st.store.settings.text_scale = ui.get_text_scale_choice().trim_end_matches('%').parse().unwrap_or(100);
    let font = ui.get_font_choice().to_string();
    st.store.settings.font = if font == SYSTEM_FONT { String::new() } else { font };
    apply_appearance(ui, &st.store.settings);
    save(&mut st);
}

const SYSTEM_FONT: &str = "Системный";

/// Common fonts that ship with each OS (nothing is bundled).
#[cfg(target_os = "macos")]
const FONTS: &[&str] = &["Helvetica Neue", "Avenir Next", "Futura", "Gill Sans", "Menlo"];
#[cfg(windows)]
const FONTS: &[&str] = &["Segoe UI", "Arial", "Verdana", "Tahoma", "Consolas"];
#[cfg(not(any(target_os = "macos", windows)))]
const FONTS: &[&str] = &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Ubuntu", "DejaVu Sans Mono"];

const THEMES: [(&str, &str, i32); 3] = [("dark", "Тёмная", 1), ("light", "Светлая", 2), ("system", "Как в системе", 0)];

fn apply_appearance(ui: &AppWindow, s: &duoray_core::store::Settings) {
    let mode = THEMES.iter().find(|t| t.0 == s.theme).map_or(1, |t| t.2);
    ui.set_theme_mode(mode);
    ui.global::<Theme>().set_text_scale(s.text_scale.clamp(80, 150) as f32 / 100.0);
    ui.set_font_family(s.font.clone().into());
}

/// Measures the active connection through xray's own SOCKS inbound (no extra
/// xray) and shows it in the connection card and on the server's row.
fn measure_live_ping(ui: &AppWindow, app: &Shared, st: &mut App) {
    let (ConnState::Connected { key, .. }, Some(conn)) = (&st.conn_state, &st.conn) else { return };
    if st.live_ping_busy {
        return;
    }
    st.live_ping_busy = true;
    let (key, generation) = (key.clone(), st.conn_gen);
    let (socks, user, pass) = (conn.socks, conn.user.clone(), conn.pass.clone());
    let url = st.store.settings.ping.url.clone();
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let result = ping::via_socks(socks, &user, &pass, &url, false, std::time::Duration::from_secs(5));
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.live_ping_busy = false;
            // Server switched or disconnected meanwhile: drop the stale reading.
            if st.conn_gen != generation || !matches!(st.conn_state, ConnState::Connected { .. }) {
                return;
            }
            let entry = match result {
                Ok(d) => {
                    let ms = d.as_millis().max(1);
                    let level = if ms < 150 { 1 } else if ms < 400 { 2 } else { 3 };
                    (format!("{ms} мс"), level, ms)
                }
                Err(_) => ("—".to_string(), 4, u128::MAX),
            };
            st.live_ping = Some((entry.0.clone(), entry.1));
            st.ping_results.insert(key, entry);
            render(&ui, &st);
        });
    });
}

/// Shows a status line that disappears after a few seconds.
fn flash_status(ui: &AppWindow, app: &Shared, st: &mut App, msg: String) {
    st.status = msg;
    st.status_gen += 1;
    let generation = st.status_gen;
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    slint::Timer::single_shot(std::time::Duration::from_secs(4), move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        let mut st = app.lock().unwrap();
        if st.status_gen == generation {
            st.status.clear();
            render(&ui, &st);
        }
    });
}

/// Adds what the user typed or pasted: a subscription URL, or share links.
fn add_input(ui: &AppWindow, app: &Shared, device: &Arc<Device>, text: &str, name: &str) -> anyhow::Result<()> {
    if store::looks_like_links(text) {
        let (id, added) = {
            let mut st = app.lock().unwrap();
            let r = st.store.add_links(text)?;
            save(&mut st);
            r
        };
        let mut st = app.lock().unwrap();
        let index = st.store.subscriptions.iter().position(|s| s.id == id).unwrap_or(0);
        ui.set_current_sub(index as i32);
        ui.set_current_server(-1);
        let msg = match added {
            0 => "Эти серверы уже добавлены".into(),
            n => format!("Добавлено: {}", plural(n, "сервер", "сервера", "серверов")),
        };
        flash_status(ui, app, &mut st, msg);
        render(ui, &st);
        return Ok(());
    }
    let (id, index) = {
        let mut st = app.lock().unwrap();
        st.store.add(text, name).map(|id| (id, st.store.subscriptions.len() - 1))?
    };
    ui.set_current_sub(index as i32);
    ui.set_current_server(-1);
    refresh(ui, app, device, id);
    Ok(())
}

fn start_ping(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    if st.ping_progress.is_some() {
        return;
    }
    let Some(sub) = current(ui, &st) else { return };
    let jobs: Vec<(String, duoray_core::server::Server)> =
        sub.servers.iter().map(|s| (key_of(sub, s), s.clone())).collect();
    if jobs.is_empty() {
        return;
    }
    for (key, _) in &jobs {
        st.ping_results.insert(key.clone(), ("…".into(), 5, u128::MAX - 1));
    }
    st.ping_progress = Some((0, jobs.len()));
    let settings = st.store.settings.ping.clone();
    let run_dir = st.path.parent().map(|p| p.join("run")).unwrap_or_else(|| PathBuf::from("run"));
    ui.set_pinging(true);
    render(ui, &st);
    drop(st);

    let on_result = {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |key: String, result: Result<std::time::Duration, String>| {
            let app = app.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let mut st = app.lock().unwrap();
                let entry = match result {
                    Ok(d) => {
                        let ms = d.as_millis().max(1);
                        let level = if ms < 150 { 1 } else if ms < 400 { 2 } else { 3 };
                        (format!("{ms} мс"), level, ms)
                    }
                    Err(_) => ("—".to_string(), 4, u128::MAX),
                };
                st.ping_results.insert(key, entry);
                if let Some((done, _)) = st.ping_progress.as_mut() {
                    *done += 1;
                }
                render(&ui, &st);
            });
        }
    };
    let on_done = {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let mut st = app.lock().unwrap();
                st.ping_progress = None;
                ui.set_pinging(false);
                render(&ui, &st);
            });
        }
    };
    ping::run_all(jobs, settings, run_dir, on_result, on_done);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(plural(1, "сервер", "сервера", "серверов"), "1 сервер");
        assert_eq!(plural(3, "сервер", "сервера", "серверов"), "3 сервера");
        assert_eq!(plural(11, "сервер", "сервера", "серверов"), "11 серверов");
        assert_eq!(plural(56, "сервер", "сервера", "серверов"), "56 серверов");
        assert_eq!(bytes(92_156_218_401), "85.8 ГБ");
        assert_eq!(date(1_798_761_600), "01.01.2027");
    }
}

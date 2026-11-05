// Release builds on Windows have no console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use duoray_core::device::Device;
use duoray_core::identity::{self, HappSpoof, SubIdentityMode};
use duoray_core::server::display_name;
use duoray_core::store::{self, SpeedSource, Store, Subscription};
use duoray_core::subscription;
use slint::{ComponentHandle, ModelRc, VecModel, Weak};

slint::include_modules!();

mod autostart;
mod connection;
mod deeplink;
mod diag;
mod editor_ui;
mod emoji;
mod exit;
mod helper;
mod instance;
mod ping;
mod qr;
mod netcheck;
mod pinlock;
mod speed;
mod routing_ui;
mod server_edit;
#[cfg(unix)]
mod signals;
mod tray;
mod support_ui;
#[cfg(not(target_os = "macos"))]
mod sysproxy;
mod update;
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
            // Vector: drawn sharp at whatever size and scale the row needs.
            .or_insert_with(|| flags::flag_svg(code).and_then(|svg| slint::Image::load_from_svg_data(svg).ok()))
            .clone()
    })
}

/// A flag drawn at exactly `width` × width·26/36 physical pixels: Twemoji
/// flags fill the band y 5..31 of their 36×36 square, rounded corners
/// included, so rendering just that band gives a crisp, anti-aliased tile at
/// any scale (a scaled-up 36 px raster looks blocky in the software renderer).
fn flag_tile(code: &str, width: u32) -> Option<slint::Image> {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<(String, u32), Option<slint::Image>>> = Default::default();
    }
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry((code.to_string(), width))
            .or_insert_with(|| {
                use resvg::{tiny_skia, usvg};
                let tree = usvg::Tree::from_data(flags::flag_svg(code)?, &usvg::Options::default()).ok()?;
                let height = (width * 26).div_ceil(36);
                let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
                let scale = width as f32 / 36.0;
                resvg::render(&tree, tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, 0.0, -5.0 * scale), &mut pixmap.as_mut());
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(pixmap.data(), width, height);
                Some(slint::Image::from_rgba8_premultiplied(buffer))
            })
            .clone()
    })
}

/// Traffic through xray's SOCKS inbound, from its counters.
/// A server's speed test, as its row shows it.
#[derive(Clone, Copy)]
enum SpeedCell {
    Queued,
    /// Megabits a second so far; `None` before the first figure.
    Testing { down: Option<f64>, up: Option<f64> },
    Done(speed::Speed),
    Failed,
}

impl SpeedCell {
    /// (state, down, up) for the row: state 0 none, 1 queued, 2 testing,
    /// 3 done, 4 failed; figures without units, "…" while not known yet.
    fn row(cell: Option<&SpeedCell>) -> (i32, String, String) {
        let so_far = |v: Option<f64>| v.filter(|v| *v > 0.0).map_or_else(|| "…".into(), speed::number);
        match cell {
            None => (0, String::new(), String::new()),
            Some(SpeedCell::Queued) => (1, String::new(), String::new()),
            Some(SpeedCell::Testing { down, up }) => (2, so_far(*down), so_far(*up)),
            Some(SpeedCell::Done(s)) => (3, speed::number(s.down_mbps), s.up_mbps.map(speed::number).unwrap_or_default()),
            Some(SpeedCell::Failed) => (4, String::new(), String::new()),
        }
    }
}

/// The server under a running speed test, where it measures to and what it
/// does now.
struct SpeedLive {
    server: String,
    target: String,
    phase: String,
}

impl SpeedLive {
    /// "Germany-1 → Speedtest: Elisa, Tallinn · загрузка 94 Мбит/с".
    fn label(&self) -> String {
        let to = if self.target.is_empty() { String::new() } else { format!(" → {}", self.target) };
        format!("{}{to} · {}", self.server, self.phase)
    }
}

struct Traffic {
    /// The last reading: when, and the counters (up, down).
    at: std::time::Instant,
    counters: (u64, u64),
    /// Bytes this session (counters restart with xray), and the speed
    /// between the last two readings, bytes a second.
    session: (u64, u64),
    rate: (f64, f64),
}

impl Traffic {
    fn read(prev: Option<&Traffic>, now: std::time::Instant, (up, down): (u64, u64)) -> Traffic {
        let Some(p) = prev else {
            return Traffic { at: now, counters: (up, down), session: (up, down), rate: (0.0, 0.0) };
        };
        // A counter that went down means xray started again: count from zero.
        let delta = |now: u64, before: u64| now.checked_sub(before).unwrap_or(now);
        let (du, dd) = (delta(up, p.counters.0), delta(down, p.counters.1));
        let secs = now.duration_since(p.at).as_secs_f64().max(0.001);
        Traffic {
            at: now,
            counters: (up, down),
            session: (p.session.0 + du, p.session.1 + dd),
            rate: (du as f64 / secs, dd as f64 / secs),
        }
    }

    /// "↓ 2.4 МБ/с · ↑ 120.0 КБ/с · 1.2 ГБ"
    fn label(&self) -> String {
        let rate = |b: f64| format!("{}/с", bytes(b as u64));
        format!("↓ {} · ↑ {} · {}", rate(self.rate.1), rate(self.rate.0), bytes(self.session.0 + self.session.1))
    }
}

struct App {
    store: Store,
    path: PathBuf,
    /// Subscription ids currently being fetched.
    refreshing: HashSet<String>,
    /// When each subscription was last fetched (successfully or not), for auto-update pacing.
    last_attempt: std::collections::HashMap<String, std::time::Instant>,
    /// Failed fetches in a row per subscription. Only a repeated failure is
    /// reported: a single one is mostly the network not up yet after sleep
    /// or at login, and the retry 10 minutes later fixes it.
    fetch_failures: std::collections::HashMap<String, u32>,
    /// From settings.server_filter; a broken pattern leaves it off.
    filter: ServerFilter,
    status: String,
    conn: Option<Connection>,
    conn_state: ConnState,
    /// Incremented per connect attempt; stale failure reports are ignored.
    conn_gen: u64,
    /// "Отменить" sets it for the attempt in progress.
    conn_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Connect again once the helper is installed.
    connect_after_install: bool,
    /// Latest ping per server key: (label, level) as shown in the row.
    /// Latest ping per server key: (label, level, sort key in ms).
    ping_results: std::collections::HashMap<String, (String, i32, u128)>,
    /// Live pings through the tunnel that failed in a row (see
    /// [`server_unresponsive`]).
    live_failures: u32,
    /// Servers moved away from since the last answer through the tunnel.
    failover_tried: std::collections::HashSet<String>,
    /// Show servers ordered by ping instead of the panel's order.
    sort_by_ping: bool,
    /// Latency of the active connection, re-measured every 5 s: (label, level).
    live_ping: Option<(String, i32)>,
    /// A live measurement is in flight (never stack them).
    live_ping_busy: bool,
    /// Where the active connection comes out (see [`measure_exit`]): the
    /// country by Cloudflare's trace and, for a balancer, the node it picked.
    exit: Option<duoray_core::balancer::Trace>,
    exit_node: Option<String>,
    exit_busy: bool,
    /// Names of the active balancer's nodes that match a single server.
    node_names: std::collections::HashMap<String, String>,
    /// Speed test results by server key (see [`speed_jobs`]), this session.
    speed_results: std::collections::HashMap<String, SpeedCell>,
    /// Stops the running speed tests; `None` when none runs.
    speed_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// (done, total) while speed tests run.
    speed_progress: Option<(usize, usize)>,
    /// What the running speed test does, for the status line.
    speed_live: Option<SpeedLive>,
    /// Traffic through the active connection (see [`measure_traffic`]).
    traffic: Option<Traffic>,
    traffic_busy: bool,
    /// (done, total) while a ping run is active.
    ping_progress: Option<(usize, usize)>,
    /// Stops the active ping run.
    ping_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The subscription whose fastest server to connect to once the running
    /// ping ends («Подключиться к самому быстрому»).
    connect_best: Option<String>,
    /// The PIN pad: locked window, or setting the PIN (see [`pinlock`]).
    pin: pinlock::State,
    /// A render is already scheduled for incoming ping results.
    ping_render_pending: bool,
    /// Bumped per transient status message, so an old timer never clears a newer one.
    status_gen: u64,
    /// When the current xray was started (for the periodic Hysteria restart).
    xray_started: Option<std::time::Instant>,
    xray_restarting: bool,
    /// The server editor's state while it is open.
    editor: Option<editor_ui::Editor>,
    update: support_ui::UpdateState,
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
    /// Kill switch: xray is gone but the tunnel stays up, so traffic stops
    /// instead of going around the VPN, until the user disconnects.
    Blocked,
}

type Shared = Arc<Mutex<App>>;

/// Where the "Нет VPN? Купить" button leads.
const BUY_URL: &str = "https://t.me/dualizm_bot";

fn main() -> anyhow::Result<()> {
    // Without a polkit agent the GUI cannot ask for the password:
    // `sudo duoray --install-helper` does the same from a terminal.
    #[cfg(target_os = "linux")]
    match std::env::args().nth(1).as_deref() {
        Some("--install-helper") => {
            helper::install()?;
            println!("Помощник DUORAY установлен.");
            return Ok(());
        }
        Some("--uninstall-helper") => {
            helper::uninstall()?;
            println!("Помощник DUORAY удалён.");
            return Ok(());
        }
        _ => {}
    }
    // Acquire before opening shared logs/state or touching xray. An updater's
    // replacement process waits until this process finishes disconnecting.
    let path = Store::default_path()?;
    let data_dir = path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
    // The login entry proxy mode leaves while it holds the system proxy: a
    // running DUORAY still owns it, otherwise put the old settings back.
    #[cfg(not(target_os = "macos"))]
    if std::env::args().any(|arg| arg == autostart::RESTORE_PROXY_ARG) {
        if let Some(_instance) = instance::acquire(&data_dir, false)? {
            sysproxy::recover(&data_dir.join("run"));
        }
        return Ok(());
    }
    // A forgotten PIN: `duoray --reset-pin` removes the lock, with DUORAY closed
    // (a running one would save its own settings over it).
    if std::env::args().any(|arg| arg == "--reset-pin") {
        let Some(_instance) = instance::acquire(&data_dir, false)? else {
            eprintln!("DUORAY запущен: закройте его (меню в трее → Выход) и повторите.");
            return Ok(());
        };
        let mut store = Store::load(&path)?;
        store.settings.pin_lock = None;
        store.save(&path)?;
        println!("PIN-код DUORAY сброшен.");
        return Ok(());
    }
    // At login the restore entry above may hold the lock for a moment.
    let wait = autostart::launched() || std::env::args().any(|arg| arg == "--wait-for-instance");
    let Some(_instance) = instance::acquire(&data_dir, wait)? else {
        // Already running (maybe hidden in the tray): bring that window up.
        if !instance::signal(&data_dir, deeplink::from_args().as_deref()) {
            instance::already_running();
        }
        return Ok(());
    };
    // Linux and Windows: an opaque window and the software renderer.
    //
    // Opaque: winit creates windows transparent there by default, so any pixel
    // a renderer leaves undrawn shows the desktop through the window (seen with
    // FemtoVG on some Windows GPUs and with partial software repaints). Our
    // background is solid, and popups are drawn inside the window.
    //
    // Software: it needs no OpenGL, so old or broken GPU drivers and VMs work,
    // and it is fast enough (a full repaint of a 2483×1394 window, editor
    // included, runs above 130 fps). Icons are SVG images, crisp with any
    // renderer. SLINT_BACKEND still picks another one (e.g. winit-femtovg).
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        let selector = slint::BackendSelector::new().with_winit_window_attributes_hook(|a| a.with_transparent(false));
        let selector = if std::env::var_os("SLINT_BACKEND").is_none() {
            selector.backend_name("winit".into()).renderer_name("software".into())
        } else {
            selector
        };
        selector.select()?;
    }
    // Wayland app_id / X11 WM_CLASS: ties the window to duoray.desktop and its icon.
    let _ = slint::set_xdg_app_id("duoray");
    diag::init(path.parent().unwrap_or(std::path::Path::new(".")));
    diag::log(format!("start {} on {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH));
    // Died in proxy mode last time: the system proxy points at a dead port.
    #[cfg(not(target_os = "macos"))]
    sysproxy::recover(&data_dir.join("run"));
    let (store, status) = match Store::load(&path) {
        Ok(s) => (s, String::new()),
        Err(e) => (Store::default(), format!("Не удалось прочитать данные: {e:#}")),
    };
    let pings = load_pings(&path);
    let s = &store.settings;
    let filter = active_filter(s.server_filter_enabled, &s.server_filter, s.server_filter_exclude);
    let app: Shared = Arc::new(Mutex::new(App {
        store,
        path,
        refreshing: HashSet::new(),
        last_attempt: Default::default(),
        fetch_failures: Default::default(),
        filter,
        status,
        conn: None,
        conn_state: ConnState::Idle,
        conn_gen: 0,
        conn_cancel: Default::default(),
        connect_after_install: false,
        ping_results: pings,
        live_failures: 0,
        failover_tried: Default::default(),
        sort_by_ping: false,
        live_ping: None,
        live_ping_busy: false,
        exit: None,
        exit_node: None,
        exit_busy: false,
        node_names: Default::default(),
        traffic: None,
        traffic_busy: false,
        speed_results: Default::default(),
        speed_cancel: None,
        speed_progress: None,
        speed_live: None,
        ping_progress: None,
        ping_cancel: None,
        connect_best: None,
        pin: Default::default(),
        ping_render_pending: false,
        status_gen: 0,
        xray_started: None,
        xray_restarting: false,
        editor: None,
        update: Default::default(),
    }));
    let device = Arc::new(Device::detect());

    let ui = AppWindow::new()?;

    ui.set_theme_swatches(ModelRc::new(VecModel::from(THEMES.iter().map(theme_swatch).collect::<Vec<_>>())));
    ui.set_font_choices(ModelRc::new(VecModel::from(
        std::iter::once(SYSTEM_FONT).chain(FONTS.iter().copied()).map(slint::SharedString::from).collect::<Vec<_>>(),
    )));
    routing_ui::install(&ui, &app);
    editor_ui::install(&ui, &app);
    window_event_filter(&ui);
    support_ui::install(&ui, &app);
    pinlock::install(&ui, &app);
    background(&ui, &app, &data_dir);
    {
        let mut st = app.lock().unwrap();
        apply_appearance(&ui, &st.store.settings);
        if !restore_selection(&ui, &st) {
            // An empty "Мои серверы" is not listed: start on something that is.
            let shown = st.store.subscriptions.iter().position(|s| !s.is_manual() || !s.servers.is_empty());
            ui.set_current_sub(shown.map_or(-1, |i| i as i32));
        }
        pinlock::lock(&ui, &mut st);
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

    ui.on_select_manual({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let index = st.store.ensure_manual_group();
            ui.set_current_sub(index as i32);
            ui.set_current_server(-1);
            ui.set_confirm_remove_index(-1);
            restore_selection_in_current(&ui, &st);
            save(&mut st);
            render(&ui, &st);
        }
    });

    ui.on_remove_server({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |row| {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            if matches!(st.conn_state, ConnState::Connecting { .. } | ConnState::Switching { .. } | ConnState::Disconnecting) {
                return;
            }
            let selected = server_key(&ui, &st);
            let Some(sub) = current(&ui, &st).filter(|s| s.is_manual()) else { return };
            let Some(index) = usize::try_from(row).ok().and_then(|row| display_order(&st, sub).get(row).copied()) else {
                return;
            };
            let (id, removed) = (sub.id.clone(), key_of(sub, &sub.servers[index]));
            let disconnect = matches!(&st.conn_state, ConnState::Connected { key, .. } if key == &removed);
            if !st.store.remove_manual_server(&id, index) {
                return;
            }
            st.ping_results.remove(&removed);
            if st.store.settings.last_server.as_ref() == Some(&removed) {
                st.store.settings.last_server = None;
            }
            // Preserve selection by identity, including when the list is sorted by ping.
            let next_row = selected.as_deref().and_then(|key| current(&ui, &st).and_then(|sub| row_of(&st, sub, key)));
            ui.set_current_server(next_row.map_or(-1, |row| row as i32));
            // The last one gone: "Мои серверы" is no longer listed, show a subscription.
            if current(&ui, &st).is_some_and(|s| s.servers.is_empty()) {
                let first = st.store.subscriptions.iter().position(|s| !s.is_manual());
                ui.set_current_sub(first.map_or(-1, |i| i as i32));
                ui.set_current_server(-1);
                restore_selection_in_current(&ui, &st);
            }
            save(&mut st);
            render(&ui, &st);
            drop(st);
            if disconnect {
                toggle_connection(&ui, &app);
            }
        }
    });

    ui.on_toggle_connection({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || toggle_connection(&ui_weak.unwrap(), &app)
    });

    ui.on_connect_selected({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let (idle, connected) = {
                let st = app.lock().unwrap();
                (
                    matches!(st.conn_state, ConnState::Idle | ConnState::Failed(_)),
                    matches!(st.conn_state, ConnState::Connected { .. }),
                )
            };
            if idle {
                start_connect(&ui, &app);
            } else if connected {
                // Connected to another server: switch, as a click on it would.
                switch_connection(&ui, &app, false);
            }
        }
    });

    ui.on_disconnect_current({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let active = matches!(
                app.lock().unwrap().conn_state,
                ConnState::Connected { .. } | ConnState::Blocked | ConnState::Connecting { .. }
            );
            if active {
                toggle_connection(&ui, &app);
            }
        }
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
                        Err(e) => {
                            diag::auto("helper_install", &format!("{e:#}"));
                            ui.set_helper_error(format!("{e:#}").into());
                        }
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

    ui.on_open_ident({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let ui = ui_weak.unwrap();
            let st = app.lock().unwrap();
            let Some(sub) = current(&ui, &st).filter(|s| !s.is_manual()) else { return };
            let settings = &st.store.settings;
            let header = |h: &[(String, String)], key: &str| h.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
            let global = identity::request_headers(&settings.happ, &device, settings.send_device_info);
            let duoray = identity::request_headers(&HappSpoof { enabled: false, ..settings.happ.clone() }, &device, settings.send_device_info);
            let not_sent = || "не отправляется".to_string();
            ui.set_ident_settings_ua(header(&global, "User-Agent").unwrap_or_default().into());
            ui.set_ident_settings_hwid(header(&global, "x-hwid").unwrap_or_else(not_sent).into());
            ui.set_ident_default_ua(header(&duoray, "User-Agent").unwrap_or_default().into());
            ui.set_ident_default_hwid(header(&duoray, "x-hwid").unwrap_or_else(not_sent).into());
            let own = &sub.identity;
            ui.set_ident_mode(match own.mode {
                SubIdentityMode::Settings => 0,
                SubIdentityMode::Duoray => 1,
                SubIdentityMode::Happ => 2,
            });
            ui.set_ident_hwid(own.hwid.clone().into());
            ui.set_ident_ua(own.user_agent.clone().into());
            // A subscription new to Happ mode gets a device of its own, so it
            // does not look like the same phone as the others.
            let happ = own.happ.clone().unwrap_or_else(|| {
                let mut h = HappSpoof::default();
                h.randomize_device();
                h
            });
            load_sub_happ(&ui, &happ);
            ui.set_ident_error("".into());
            ui.set_ident_open(true);
        }
    });

    ui.on_ident_new_hwid({
        let ui_weak = ui.as_weak();
        move || ui_weak.unwrap().set_ident_hwid(identity::random_device_hwid().into())
    });
    ui.on_ident_new_ids({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            let mut h = read_sub_happ(&ui);
            h.randomize_ids();
            load_sub_happ(&ui, &h);
        }
    });
    ui.on_ident_random_device({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak.unwrap();
            let mut h = read_sub_happ(&ui);
            h.randomize_device();
            load_sub_happ(&ui, &h);
        }
    });

    ui.on_save_ident({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mode = match ui.get_ident_mode() {
                1 => SubIdentityMode::Duoray,
                2 => SubIdentityMode::Happ,
                _ => SubIdentityMode::Settings,
            };
            let happ = read_sub_happ(&ui);
            let (hwid, user_agent) = (ui.get_ident_hwid().trim().to_string(), ui.get_ident_ua().trim().to_string());
            let checked: Vec<(&str, &str)> = match mode {
                SubIdentityMode::Settings => vec![],
                SubIdentityMode::Duoray => vec![("HWID", &hwid), ("User-Agent", &user_agent)],
                SubIdentityMode::Happ => vec![
                    ("Версия Happ", &happ.app_version),
                    ("Версия ОС", &happ.os_version),
                    ("Модель", &happ.model),
                    ("Язык", &happ.locale),
                    ("User ID", &happ.user_id),
                    ("HWID", &happ.hwid),
                ],
            };
            for (name, value) in checked {
                if let Some(problem) = identity::header_value_problem(value) {
                    ui.set_ident_error(format!("{name}: {problem}").into());
                    return;
                }
            }
            let id = {
                let mut st = app.lock().unwrap();
                let Some(i) = usize::try_from(ui.get_current_sub()).ok() else { return };
                let Some(sub) = st.store.subscriptions.get_mut(i) else { return };
                // Fields of the other modes are kept, so switching back restores them.
                let mut own = sub.identity.clone();
                own.mode = mode;
                match mode {
                    SubIdentityMode::Settings => {}
                    SubIdentityMode::Duoray => (own.hwid, own.user_agent) = (hwid, user_agent),
                    SubIdentityMode::Happ => own.happ = Some(happ),
                }
                let changed = own != sub.identity;
                sub.identity = own;
                let id = sub.id.clone();
                if changed {
                    save(&mut st);
                }
                changed.then_some(id)
            };
            ui.set_ident_open(false);
            // The panel may answer differently to the new identity.
            if let Some(id) = id {
                refresh(&ui, &app, &device, id);
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

    // ── Диагностика сети ────────────────────────────────────────
    ui.on_run_diagnostics({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            if ui.get_diag_running() {
                return;
            }
            let input = {
                let st = app.lock().unwrap();
                let server = selected_server(&ui, &st).filter(|s| s.proxies <= 1 && !s.address.is_empty()).map(|s| {
                    let tcp = s.protocol != "Hysteria2" && !matches!(s.network.as_str(), "kcp" | "quic");
                    (s.address.clone(), s.port, tcp)
                });
                let vpn = match (&st.conn_state, &st.conn) {
                    (ConnState::Connected { .. }, Some(c)) => Some((c.socks, c.user.clone(), c.pass.clone())),
                    _ => None,
                };
                netcheck::Input {
                    subscription: current(&ui, &st).filter(|s| !s.is_manual()).map(|s| s.url.clone()),
                    server,
                    vpn,
                    ping_url: st.store.settings.ping.url.clone(),
                }
            };
            let rows: Vec<DiagRow> =
                netcheck::TITLES.iter().map(|t| DiagRow { state: netcheck::Level::Running as i32, title: (*t).into(), detail: "".into() }).collect();
            ui.set_diag_rows(ModelRc::new(VecModel::from(rows)));
            ui.set_diag_running(true);
            ui.set_diag_open(true);
            let ui_weak = ui.as_weak();
            std::thread::spawn(move || {
                netcheck::run(input, |i, level, detail| {
                    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                        use slint::Model;
                        let rows = ui.get_diag_rows();
                        if let Some(mut row) = rows.row_data(i) {
                            (row.state, row.detail) = (level as i32, detail.into());
                            rows.set_row_data(i, row);
                        }
                    });
                });
                let _ = ui_weak.upgrade_in_event_loop(|ui| ui.set_diag_running(false));
            });
        }
    });
    ui.on_copy_diagnostics({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            use slint::Model;
            let ui = ui_weak.unwrap();
            let level = |state: i32| match state {
                1 => netcheck::Level::Ok,
                2 => netcheck::Level::Warn,
                3 => netcheck::Level::Fail,
                4 => netcheck::Level::Skip,
                _ => netcheck::Level::Running,
            };
            let rows: Vec<_> = ui.get_diag_rows().iter().map(|r| (level(r.state), r.title.to_string(), r.detail.to_string())).collect();
            let copied = arboard::Clipboard::new().and_then(|mut c| c.set_text(netcheck::as_text(&rows))).is_ok();
            let mut st = app.lock().unwrap();
            flash_status(&ui, &app, &mut st, if copied { "Результат скопирован".into() } else { "Не удалось скопировать".into() });
            render(&ui, &st);
        }
    });

    // ── Moving settings to another computer ─────────────────────
    ui.on_export_settings({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let dir = downloads_dir();
            let path = dir.join(format!("duoray-settings-{}.json", chrono::Local::now().format("%Y-%m-%d")));
            let text = serde_json::to_string_pretty(&st.store.export()).unwrap_or_default();
            let msg = match std::fs::write(&path, text) {
                Ok(()) => {
                    open_path(&dir);
                    format!("Настройки сохранены в {}", path.display())
                }
                Err(e) => format!("Не удалось сохранить настройки: {e}"),
            };
            flash_status_for(&ui, &app, &mut st, msg, 8);
            render(&ui, &st);
        }
    });
    // From the clipboard: the file's text, or the file itself copied in a
    // file manager (its path or file:// URI).
    ui.on_import_settings({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let ui = ui_weak.unwrap();
            let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
            let line = text.lines().next().unwrap_or_default().trim();
            let file = url::Url::parse(line).ok().and_then(|u| u.to_file_path().ok()).or_else(|| Some(PathBuf::from(line)));
            match file.filter(|p| p.is_file()) {
                Some(path) => import_file(&ui, &app, &device, &path),
                None => import_settings(&ui, &app, &device, &text),
            }
        }
    });
    // A file dropped onto the window.
    ui.on_import_file({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move |path| import_file(&ui_weak.unwrap(), &app, &device, std::path::Path::new(path.as_str()))
    });

    // ── Settings page ───────────────────────────────────────────
    ui.on_open_settings({
        let (ui_weak, app, device) = (ui.as_weak(), app.clone(), device.clone());
        move || {
            let ui = ui_weak.unwrap();
            ui.set_duoray_ua(identity::DUORAY_USER_AGENT.into());
            ui.set_duoray_hwid(device.hwid.clone().into());
            ui.set_settings_helper_status(helper::describe().into());
            ui.set_settings_helper_installed(helper::installed());
            let st = app.lock().unwrap();
            load_happ(&ui, &st.store.settings.happ);
            load_ping(&ui, &st.store.settings.ping);
            ui.set_speedtest_source(match st.store.settings.speedtest {
                SpeedSource::Ookla => 0,
                SpeedSource::Cloudflare => 1,
                SpeedSource::Own => 2,
            });
            ui.set_speedtest_url(st.store.settings.speedtest_url.clone().into());
            ui.set_send_device_info(st.store.settings.send_device_info);
            ui.set_hysteria_restart(st.store.settings.hysteria_restart);
            ui.set_hysteria_minutes(st.store.settings.hysteria_restart_minutes.to_string().into());
            ui.set_telemetry_enabled(st.store.settings.telemetry == Some(true));
            ui.set_auto_update(st.store.settings.auto_update);
            ui.set_switch_on_select(st.store.settings.switch_on_select);
            ui.set_keep_connected(st.store.settings.keep_connected);
            ui.set_pin_enabled(st.store.settings.pin_lock.is_some());
            ui.set_close_to_tray(st.store.settings.close_to_tray);
            ui.set_autostart(st.store.settings.autostart);
            ui.set_autoconnect(st.store.settings.autoconnect);
            ui.set_kill_switch(st.store.settings.kill_switch);
            ui.set_proxy_mode(st.store.settings.proxy_mode);
            ui.set_proxy_port(st.store.settings.proxy_port.to_string().into());
            ui.set_duoray_version(format!("{} ({})", env!("CARGO_PKG_VERSION"), update::BUILD).into());
            ui.set_text_scale_choice(format!("{}%", st.store.settings.text_scale).into());
            let theme = theme_of(&st.store.settings.theme).1;
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
                    ui.set_settings_helper_installed(helper::installed());
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

    ui.on_speed_test_all({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            if let Some(cancel) = &st.speed_cancel {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
            let Some(sub) = current(&ui, &st) else { return };
            let jobs: Vec<_> = sub.servers.iter().map(|s| (key_of(sub, s), s.clone())).collect();
            if jobs.is_empty() {
                return;
            }
            st.status.clear();
            speed_jobs(&ui, &app, st, jobs);
        }
    });

    ui.on_show_sub_qr({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let Some(sub) = current(&ui, &st).filter(|s| !s.is_manual()) else { return };
            let (title, url) = (sub.display_name(), sub.url.clone());
            show_qr(&ui, &app, &mut st, &title, &url);
        }
    });
    ui.on_qr_copy({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let msg = match arboard::Clipboard::new().and_then(|mut c| c.set_text(ui.get_qr_text().to_string())) {
                Ok(()) => "Скопировано в буфер обмена".into(),
                Err(e) => format!("Не удалось скопировать: {e}"),
            };
            flash_status(&ui, &app, &mut st, msg);
            render(&ui, &st);
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
    ui.set_filter_text(app.lock().unwrap().store.settings.server_filter.clone().into());
    ui.set_filter_exclude(app.lock().unwrap().store.settings.server_filter_exclude);
    ui.on_filter_changed({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let (pattern, exclude) = (ui.get_filter_text().to_string(), ui.get_filter_exclude());
            match ServerFilter::new(&pattern, exclude) {
                Ok(f) => {
                    st.filter = f;
                    ui.set_filter_error("".into());
                }
                Err(e) => {
                    // Show everything until the pattern is fixed.
                    st.filter = ServerFilter { re: None, exclude };
                    ui.set_filter_error(e.into());
                }
            }
            st.store.settings.server_filter = pattern;
            st.store.settings.server_filter_exclude = exclude;
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
    ui.on_open_link(|url| open_url(&url));
    ui.on_ping_current({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || start_ping(&ui_weak.unwrap(), &app)
    });
    ui.on_cancel_ping({
        let app = app.clone();
        move || {
            let mut st = app.lock().unwrap();
            // Stopped: no connecting to the fastest either.
            st.connect_best = None;
            if let Some(c) = &st.ping_cancel {
                c.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    ui.on_connect_best({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let ui = ui_weak.unwrap();
            let mut st = app.lock().unwrap();
            let Some(sub) = current(&ui, &st) else { return };
            if st.ping_progress.is_some() {
                flash_status(&ui, &app, &mut st, "Дождитесь конца пинга".into());
                return render(&ui, &st);
            }
            st.connect_best = Some(sub.id.clone());
            drop(st);
            start_ping(&ui, &app);
        }
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

    // While connected: latency through the live tunnel every 5 s while the
    // window is shown. Hidden (the tray, the dock), nobody sees the figure:
    // every 10 s, and only to keep the connection.
    let live_ping_timer = slint::Timer::default();
    live_ping_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(5), {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        let odd_tick = std::cell::Cell::new(false);
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            odd_tick.set(!odd_tick.get());
            let shown = ui.window().is_visible() && !ui.window().is_minimized();
            let mut st = app.lock().unwrap();
            if shown || (st.store.settings.keep_connected && odd_tick.get()) {
                measure_live_ping(&ui, &app, &mut st);
            }
        }
    });

    // While connected: speed and traffic every second, window shown.
    let traffic_timer = slint::Timer::default();
    traffic_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if !ui.window().is_visible() || ui.window().is_minimized() {
                return;
            }
            let mut st = app.lock().unwrap();
            measure_traffic(&ui, &app, &mut st);
        }
    });

    // While connected: exit country and balancer node every 30 s, window shown.
    let exit_timer = slint::Timer::default();
    exit_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(30), {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if !ui.window().is_visible() || ui.window().is_minimized() {
                return;
            }
            let mut st = app.lock().unwrap();
            measure_exit(&ui, &app, &mut st);
        }
    });

    // Hysteria2 can stall after a while: optionally restart xray on a timer.
    let hysteria_timer = slint::Timer::default();
    hysteria_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(15), {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            restart_hysteria_if_due(&ui, &app);
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
                // "завтра" turns into "сегодня" without any update.
                render_expiry(&ui, &st);
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
    // (Not for the README demo: its subscription is made up.)
    let ids: Vec<String> = app.lock().unwrap().store.subscriptions.iter().map(|s| s.id.clone()).collect();
    for id in ids.into_iter().filter(|_| std::env::var_os("DUORAY_DEMO").is_none()) {
        refresh(&ui, &app, &device, id);
    }

    // Dev hook, README screenshot: with DUORAY_DEMO (and the store from
    // `cargo run -p duoray-core --example demo_store`), one frame of the window
    // as connected, nothing fetched or pinged, saved as <prefix>.demo.png.
    if let (Some(prefix), Some(_)) = (std::env::var_os("DUORAY_SNAPSHOT"), std::env::var_os("DUORAY_DEMO")) {
        let (ui_weak, prefix, app) = (ui.as_weak(), PathBuf::from(prefix), app.clone());
        ui.window().set_size(slint::LogicalSize::new(1290.0, 820.0));
        // Time for a tiling compositor's size to be set by hand (niri msg).
        slint::Timer::single_shot(std::time::Duration::from_secs(5), move || {
            let ui = ui_weak.unwrap();
            ui.set_conn_state("connected".into());
            ui.set_conn_title("Подключено".into());
            ui.set_conn_server("Wi-Fi • Авто-выбор".into());
            ui.set_conn_node("Нидерланды (Амстердам)".into());
            ui.set_conn_exit_text("Нидерланды".into());
            ui.set_conn_exit_flag(flag_tile("NL", (42.0 * ui.window().scale_factor()).round() as u32).unwrap_or_default());
            ui.set_conn_ping_text("38 мс".into());
            ui.set_conn_ping_level(1);
            // Speed results in every state, and one test under way.
            {
                let mut st = app.lock().unwrap();
                let done = |down, up| SpeedCell::Done(speed::Speed { down_mbps: down, up_mbps: up });
                let cells = [
                    done(212.4, Some(87.3)),
                    done(94.0, Some(101.2)),
                    SpeedCell::Testing { down: Some(173.0), up: None },
                    SpeedCell::Queued,
                    SpeedCell::Failed,
                    done(8.24, Some(3.1)),
                ];
                if let Some(sub) = current(&ui, &st) {
                    let keys: Vec<String> = sub.servers.iter().map(|s| key_of(sub, s)).collect();
                    st.store.settings.favorites = [0, 3].iter().filter_map(|&i| keys.get(i).cloned()).collect();
                    st.speed_results.extend(keys.into_iter().zip(cells));
                }
                st.speed_progress = Some((2, 6));
                st.speed_live = Some(SpeedLive {
                    server: "Германия (Франкфурт)".into(),
                    target: "Speedtest: Deutsche Telekom, Frankfurt".into(),
                    phase: "загрузка 173 Мбит/с".into(),
                });
                render(&ui, &st);
            }
            let ui_weak = ui.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                snapshot(ui_weak.unwrap().window(), &prefix.with_extension("demo.png"));
                let _ = slint::quit_event_loop();
            });
        });
    } else if let Some(prefix) = std::env::var_os("DUORAY_SNAPSHOT") {
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
            // The section with the theme tiles.
            ui.set_settings_section(2);
            let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
            slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                let ui = ui_weak.unwrap();
                snapshot(ui.window(), &prefix.with_extension("settings.png"));
                ui.set_settings_open(false);
                ui.set_routing_open(true);
                // Hide the long games list so the apps block fits in the shot.
                ui.set_routing_games(ModelRc::new(VecModel::from(Vec::<GameRow>::new())));
                ui.set_routing_app_mode(1);
                ui.invoke_routing_changed();
                ui.invoke_routing_app_added("Steam".into());
                ui.invoke_routing_pick_running();
                ui.set_routing_picking(true);
                let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                    let ui = ui_weak.unwrap();
                    snapshot(ui.window(), &prefix.with_extension("routing.png"));
                    ui.set_routing_geo_search_query("ru".into());
                    ui.set_routing_geo_results(ModelRc::new(VecModel::from(vec![
                        GeoRow { source: "geosite".into(), tag: "category-ru".into(), value: "domain:vk.com".into() },
                        GeoRow { source: "geosite".into(), tag: "category-ru".into(), value: "full:пример.рф".into() },
                        GeoRow { source: "geoip".into(), tag: "ru".into(), value: "1.2.3.0/24".into() },
                    ])));
                    ui.set_routing_geo_search_status("Найдено: 3".into());
                    let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                    slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                        let ui = ui_weak.unwrap();
                        ui.invoke_routing_show_geo();
                        snapshot(ui.window(), &prefix.with_extension("routing-search.png"));
                        ui.set_routing_advanced(true);
                        ui.invoke_routing_changed();
                        ui.invoke_routing_add_rule();
                        let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                        slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                            let ui = ui_weak.unwrap();
                            snapshot(ui.window(), &prefix.with_extension("routing-advanced.png"));
                            // Server editor: the first server's config, then a broken edit of it.
                            ui.set_routing_open(false);
                            ui.invoke_edit_server(0);
                            let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                            slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                                let ui = ui_weak.unwrap();
                                snapshot(ui.window(), &prefix.with_extension("editor.png"));
                                let broken = ui.get_editor_json().replacen('"', "", 1);
                                ui.set_editor_json(broken.as_str().into());
                                ui.invoke_editor_json_changed(broken.into());
                                let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                                slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                                    let ui = ui_weak.unwrap();
                                    snapshot(ui.window(), &prefix.with_extension("editor-error.png"));
                                    ui.invoke_editor_cancel();
                                    ui.invoke_edit_server(1);
                                    let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                                    slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                                        let ui = ui_weak.unwrap();
                                        snapshot(ui.window(), &prefix.with_extension("editor-form.png"));
                                        // Save a changed port: the row gets the "изменён" chip.
                                        ui.invoke_editor_field_changed(2, "9443".into());
                                        ui.invoke_editor_save();
                                        let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                                        slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                                            let ui = ui_weak.unwrap();
                                            snapshot(ui.window(), &prefix.with_extension("edited.png"));
                                            // The subscription's identity dialog in Happ mode (not saved).
                                            ui.invoke_open_ident();
                                            ui.set_ident_mode(2);
                                            let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                                            slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
                                                let ui = ui_weak.unwrap();
                                                snapshot(ui.window(), &prefix.with_extension("ident.png"));
                                                // The status card as connected through a balancer (set by hand: no tunnel here).
                                                ui.set_ident_open(false);
                                                ui.set_conn_state("connected".into());
                                                ui.set_conn_title("Подключено".into());
                                                ui.set_conn_detail("LTE x1 • Авто-выбор · DUORAY".into());
                                                ui.set_conn_node("LTE #2 (1x)".into());
                                                ui.set_conn_exit_flag(flag_tile("PL", (42.0 * ui.window().scale_factor()).round() as u32).unwrap_or_default());
                                                ui.set_conn_server("LTE x1 • Авто-выбор".into());
                                                ui.set_conn_ping_text("94 мс".into());
                                                ui.set_conn_ping_level(1);
                                                ui.set_conn_exit_text("Польша".into());
                                                let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                                                slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                                                    let ui = ui_weak.unwrap();
                                                    snapshot(ui.window(), &prefix.with_extension("connected.png"));
                                                    ui.set_conn_state("failed".into());
                                                    ui.set_conn_title("Ошибка подключения".into());
                                                    ui.set_conn_detail("preparing xray config: cannot resolve rubridge-3.pullk.in: failed to lookup address information: nodename nor servname provided, or not known".into());
                                                    ui.set_conn_ping_level(0);
                                                    let (ui_weak, prefix) = (ui.as_weak(), prefix.clone());
                                                    slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                                                        let ui = ui_weak.unwrap();
                                                        snapshot(ui.window(), &prefix.with_extension("connection-error.png"));
                                                        ui.invoke_show_sub_qr();
                                                        let ui_weak = ui.as_weak();
                                                        slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                                                            snapshot(ui_weak.unwrap().window(), &prefix.with_extension("qr.png"));
                                                            let _ = slint::quit_event_loop();
                                                        });
                                                    });
                                                });
                                            });
                                        });
                                    });
                                });
                            });
                        });
                    });
                });
            });
        });
    }

    // At login: straight to the tray (the window appears if there is none).
    let hidden = autostart::launched() && app.lock().unwrap().store.settings.close_to_tray;
    if !hidden {
        show_window(&ui);
    }
    {
        let st = app.lock().unwrap();
        // The entry carries the program path, which updates can change.
        if st.store.settings.autostart
            && let Err(e) = autostart::set(true)
        {
            diag::log(format!("autostart: {e:#}"));
        }
        if st.store.settings.autoconnect && deeplink::from_args().is_none() {
            autoconnect(&ui, &app, 0);
        }
    }
    // Started by a duoray:// link (the first launch; later ones go through
    // the instance port).
    if let Some(link) = deeplink::from_args() {
        open_link(&ui, &link);
    }
    #[cfg(unix)]
    signals::quit_on_termination();
    // Not ui.run(): with the tray, hiding the window does not end the app.
    slint::run_event_loop_until_quit()?;

    // Quit: bring the network back before exiting.
    let conn = app.lock().unwrap().conn.take();
    if let Some(conn) = conn {
        conn.disconnect();
    }
    Ok(())
}

/// The tray icon, closing to it, and later launches showing this window.
fn background(ui: &AppWindow, app: &Shared, data_dir: &std::path::Path) {
    let ui_weak = ui.as_weak();
    instance::serve(data_dir, move |link| {
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            show_window(&ui);
            if let Some(link) = link {
                open_link(&ui, &link);
            }
        });
    });

    ui.window().on_close_requested({
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let mut st = app.lock().unwrap();
            if !(tray::available() && st.store.settings.close_to_tray) {
                let _ = slint::quit_event_loop();
            }
            // Hidden to the tray: opening it again asks for the PIN.
            if let Some(ui) = ui_weak.upgrade() {
                pinlock::lock(&ui, &mut st);
            }
            slint::CloseRequestResponse::HideWindow
        }
    });

    // After the event loop starts: macOS wants a running NSApp for the icon.
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    slint::Timer::single_shot(std::time::Duration::ZERO, move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        let started = start_tray(&ui, &app);
        diag::log(format!("tray: {}", if started { "on" } else { "not available" }));
        // Started hidden at login, but there is no tray to bring it back from.
        if !started && !ui.window().is_visible() {
            show_window(&ui);
        }
        if !started {
            retry_tray(&ui, &app);
        }
    });
}

/// Creates the tray icon; false if the desktop has no tray (yet).
fn start_tray(ui: &AppWindow, app: &Shared) -> bool {
    let state = tray_state(ui, &app.lock().unwrap());
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    let started = tray::start(state, move |action| {
        let app = app.clone();
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            // Locked: connecting or switching from the tray asks for the PIN
            // first. Quitting does not (the process can be ended anyway).
            if !matches!(action, tray::Action::Show | tray::Action::Quit) && pinlock::is_locked(&app.lock().unwrap()) {
                return show_window(&ui);
            }
            match action {
                tray::Action::Show => show_window(&ui),
                tray::Action::Toggle => ui.invoke_toggle_connection(),
                tray::Action::Server(key) => connect_to(&ui, &app, key),
                tray::Action::Quit => {
                    let _ = slint::quit_event_loop();
                }
            }
        });
    });
    ui.set_tray_available(started);
    started
}

/// At login the desktop may bring up its tray after us (COSMIC on Pop!_OS
/// did: "tray: not available", then no icon for the whole session, and
/// closing the window quit the app). Tries again every 3 s for two minutes.
fn retry_tray(ui: &AppWindow, app: &Shared) {
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    let tries = std::cell::Cell::new(0);
    let timer = std::rc::Rc::new(slint::Timer::default());
    let handle = timer.clone();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(3), move || {
        let Some(ui) = ui_weak.upgrade() else { return handle.stop() };
        tries.set(tries.get() + 1);
        if start_tray(&ui, &app) {
            diag::log(format!("tray: on after {} s", tries.get() * 3));
            handle.stop();
        } else if tries.get() >= 40 {
            handle.stop();
        }
    });
}

fn show_window(ui: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    let _ = ui.show();
    // Started hidden at login, Slint still creates the window, unmapped. On
    // Wayland an unmapped window cannot be shown later (winit's set_visible
    // does nothing there), so the window never came up, from the tray or a
    // second launch (Pop!_OS / COSMIC, niri). Hiding destroys it on Wayland
    // and showing again creates it mapped.
    //
    // Wayland also lets no app raise its own window without an activation
    // token from the user's click, and tray menus pass none (niri with
    // Noctalia): a window open behind others or on another workspace stayed
    // there. niri, Hyprland and sway focus it when asked over their IPC;
    // elsewhere a window mapped anew is usually focused, so it is made anew.
    static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if is_wayland(ui) {
        let first = !SHOWN.swap(true, std::sync::atomic::Ordering::Relaxed);
        let focused = ui.window().with_winit_window(|w| w.has_focus()).unwrap_or(false);
        if first || (!focused && !compositor_can_focus()) {
            let _ = ui.hide();
            let _ = ui.show();
        }
        if !focused && compositor_can_focus() {
            // Once the window is mapped.
            slint::Timer::single_shot(std::time::Duration::from_millis(200), || {
                std::thread::spawn(compositor_focus);
            });
        }
    }
    ui.window().with_winit_window(|w| {
        w.set_minimized(false);
        w.set_visible(true);
        w.focus_window();
    });
}

/// A compositor that focuses a window on request over its IPC: niri,
/// Hyprland, sway.
fn compositor_can_focus() -> bool {
    cfg!(target_os = "linux") && ["NIRI_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE", "SWAYSOCK"].iter().any(|v| std::env::var_os(v).is_some())
}

/// Asks the compositor to focus our window, found by our process id.
fn compositor_focus() {
    let pid = std::process::id();
    let run = |program: &str, args: &[&str]| std::process::Command::new(program).args(args).output().ok().filter(|o| o.status.success());
    if std::env::var_os("NIRI_SOCKET").is_some() {
        let Some(out) = run("niri", &["msg", "--json", "windows"]) else { return };
        let windows: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        if let Some(id) = windows.iter().find(|w| w["pid"].as_u64() == Some(u64::from(pid))).and_then(|w| w["id"].as_u64()) {
            run("niri", &["msg", "action", "focus-window", "--id", &id.to_string()]);
        }
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        run("hyprctl", &["dispatch", "focuswindow", &format!("pid:{pid}")]);
    } else if std::env::var_os("SWAYSOCK").is_some() {
        run("swaymsg", &[&format!("[pid={pid}]"), "focus"]);
    }
}

fn is_wayland(ui: &AppWindow) -> bool {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    ui.window()
        .with_winit_window(|w| w.window_handle().is_ok_and(|h| matches!(h.as_raw(), RawWindowHandle::Wayland(_))))
        .unwrap_or(false)
}

fn tray_state(ui: &AppWindow, st: &App) -> tray::State {
    let toggle = match st.conn_state {
        ConnState::Connected { .. } | ConnState::Connecting { .. } | ConnState::Switching { .. } | ConnState::Blocked => {
            "Отключиться"
        }
        ConnState::Disconnecting => "",
        ConnState::Idle | ConnState::Failed(_) if selected_server(ui, st).is_some() => "Подключиться",
        ConnState::Idle | ConnState::Failed(_) => "",
    };
    let current = match &st.conn_state {
        ConnState::Connected { key, .. } => Some(key.as_str()),
        _ => st.store.settings.last_server.as_deref(),
    };
    // Starred servers that still exist, in the order starred.
    let servers = st
        .store
        .settings
        .favorites
        .iter()
        .filter_map(|key| {
            let sub = st.store.subscriptions.iter().find(|s| key.starts_with(&format!("{}\u{1f}", s.id)))?;
            let server = sub.servers.iter().find(|s| key_of(sub, s) == *key)?;
            Some(tray::Server { key: key.clone(), label: display_name(&server.name).1, current: current == Some(key.as_str()) })
        })
        .collect();
    tray::State { connected: matches!(st.conn_state, ConnState::Connected { .. }), status: ui.get_conn_title().to_string(), toggle: toggle.into(), servers }
}

/// A server picked in the tray menu: selected, and connected to (or switched
/// to, while connected).
fn connect_to(ui: &AppWindow, app: &Shared, key: String) {
    let mut st = app.lock().unwrap();
    st.store.settings.last_server = Some(key);
    save(&mut st);
    if !restore_selection(ui, &st) {
        return;
    }
    let idle = matches!(st.conn_state, ConnState::Idle | ConnState::Failed(_));
    render(ui, &st);
    drop(st);
    if idle {
        toggle_connection(ui, app);
    } else {
        switch_connection(ui, app, false);
    }
}

/// What reports say about the setup: switches and the kind of server in use,
/// never its address, name, keys or the subscription link.
fn diag_context(ui: &AppWindow, st: &App) -> serde_json::Value {
    let s = &st.store.settings;
    let server = selected_server(ui, st).map(|v| {
        serde_json::json!({
            "protocol": v.protocol, "network": v.network, "security": v.security, "proxies": v.proxies,
            "json": matches!(v.source, duoray_core::server::Source::Json(_)), "edited": v.override_key.is_some(),
        })
    });
    let conn = match &st.conn_state {
        ConnState::Idle => "idle",
        ConnState::Connecting { .. } => "connecting",
        ConnState::Connected { .. } => "connected",
        ConnState::Switching { .. } => "switching",
        ConnState::Disconnecting => "disconnecting",
        ConnState::Failed(_) => "failed",
        ConnState::Blocked => "blocked",
    };
    serde_json::json!({
        "connection": conn,
        "server": server,
        "subscriptions": st.store.subscriptions.iter().filter(|x| !x.is_manual()).count(),
        "renderer": std::env::var("SLINT_BACKEND")
            .unwrap_or_else(|_| if cfg!(any(target_os = "linux", windows)) { "software" } else { "default" }.into()),
        "settings": {
            "ping": format!("{:?}/{}", s.ping.mode, s.ping.threads),
            "routing_advanced": s.routing.advanced,
            "ru_direct": s.routing.simple.ru_direct,
            "whitelist_direct": s.routing.simple.whitelist_direct,
            "lan_direct": s.routing.simple.lan_direct,
            "games_direct": s.routing.simple.games.len(),
            "apps_mode": format!("{:?}", s.routing.apps.mode),
            "hysteria_restart": s.hysteria_restart,
            "send_device_info": s.send_device_info,
            "happ_spoof": s.happ.enabled,
        },
    })
}

/// Indices into `sub.servers` in the order the list shows them. Row numbers in
/// the UI always refer to this order.
/// The server list filter: a case-insensitive regular expression over the
/// server's name and description, keeping only the matches or hiding them.
struct ServerFilter {
    re: Option<regex::Regex>,
    exclude: bool,
}

impl ServerFilter {
    /// A blank pattern is no filter; a broken one is an error to show.
    fn new(pattern: &str, exclude: bool) -> Result<Self, String> {
        let pattern = pattern.trim();
        let re = if pattern.is_empty() {
            None
        } else {
            Some(regex::RegexBuilder::new(pattern).case_insensitive(true).build().map_err(|e| match e {
                regex::Error::Syntax(_) => "Ошибка в выражении".to_string(),
                other => other.to_string(),
            })?)
        };
        Ok(Self { re, exclude })
    }

    fn shows(&self, name: &str, description: &str) -> bool {
        self.re.as_ref().is_none_or(|re| (re.is_match(name) || re.is_match(description)) != self.exclude)
    }
}

/// The filter the list applies: none while the feature is off or the pattern is broken.
fn active_filter(enabled: bool, pattern: &str, exclude: bool) -> ServerFilter {
    if !enabled {
        return ServerFilter { re: None, exclude };
    }
    ServerFilter::new(pattern, exclude).unwrap_or(ServerFilter { re: None, exclude })
}

fn display_order(st: &App, sub: &Subscription) -> Vec<usize> {
    let connected = match &st.conn_state {
        ConnState::Connected { key, .. } => Some(key.as_str()),
        _ => None,
    };
    let mut order: Vec<usize> = (0..sub.servers.len())
        .filter(|&i| {
            let s = &sub.servers[i];
            // The connected server never disappears from the list.
            st.filter.shows(&s.name, s.description.as_deref().unwrap_or_default())
                || connected == Some(key_of(sub, s).as_str())
        })
        .collect();
    if st.sort_by_ping {
        // The connected server stays on top: it would jump up and down as
        // pings come in. Stable: equal pings keep the panel's order.
        // Unmeasured and failed go last.
        order.sort_by_key(|&i| {
            let key = key_of(sub, &sub.servers[i]);
            (connected != Some(key.as_str()), st.ping_results.get(&key).map_or(u128::MAX, |p| p.2))
        });
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

/// A ping row: label, level (1 fast … 3 slow, 4 failed) and the sort key.
fn ping_entry(ms: Option<u128>) -> (String, i32, u128) {
    match ms {
        Some(ms) => {
            let ms = ms.max(1);
            let level = if ms < 150 { 1 } else if ms < 400 { 2 } else { 3 };
            (format!("{ms} мс"), level, ms)
        }
        None => ("—".to_string(), 4, u128::MAX),
    }
}

/// Last ping results live in pings.json next to the store, so they are
/// still shown after a restart (people ping the same locations again).
/// Server key -> milliseconds, or null when the server did not answer.
fn pings_path(store_path: &std::path::Path) -> PathBuf {
    store_path.with_file_name("pings.json")
}

fn load_pings(store_path: &std::path::Path) -> std::collections::HashMap<String, (String, i32, u128)> {
    let Ok(text) = std::fs::read_to_string(pings_path(store_path)) else { return Default::default() };
    let saved: std::collections::HashMap<String, Option<u64>> = serde_json::from_str(&text).unwrap_or_default();
    saved.into_iter().map(|(k, ms)| (k, ping_entry(ms.map(u128::from)))).collect()
}

/// Saves finished results of servers that still exist; "…" (in progress) is skipped.
fn save_pings(st: &App) {
    if std::env::var_os("DUORAY_SNAPSHOT").is_some() {
        return;
    }
    let live: HashSet<String> =
        st.store.subscriptions.iter().flat_map(|sub| sub.servers.iter().map(|s| key_of(sub, s))).collect();
    let saved: std::collections::BTreeMap<&String, Option<u64>> = st
        .ping_results
        .iter()
        .filter(|(k, r)| r.1 != 5 && live.contains(*k))
        .map(|(k, r)| (k, (r.1 != 4).then(|| u64::try_from(r.2).unwrap_or(u64::MAX))))
        .collect();
    let path = pings_path(&st.path);
    let tmp = path.with_extension("json.tmp");
    let ok = serde_json::to_vec(&saved).is_ok_and(|bytes| std::fs::write(&tmp, bytes).is_ok());
    if !ok || std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
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

/// While connected, picking another server switches the connection to it,
/// unless the user turned that off.
fn follow_selection(ui: &AppWindow, app: &Shared) {
    if !app.lock().unwrap().store.settings.switch_on_select {
        return;
    }
    switch_connection(ui, app, false);
}

/// Restarts the active connection (e.g. the routing changed).
fn reconnect(ui: &AppWindow, app: &Shared) {
    switch_connection(ui, app, true);
}

/// Winit-level fixes for the main window, registered once the winit window
/// exists (a window has one filter, so they share it):
/// - Shortcuts on non-Latin layouts. Slint matches Ctrl+V/C/X/A/Z by the
///   typed letter, so on a Russian layout Ctrl+V arrives as Ctrl+"м": fields
///   did not paste, and the key fell through to "add from clipboard". Ctrl/Cmd
///   with a letter key is passed on as the Latin letter of that key.
/// - After the window was minimized, covered or unfocused, repaint it whole:
///   the software renderer otherwise presents only what changed, and Windows
///   may have dropped the rest (parts of the window stayed blank).
fn window_event_filter(ui: &AppWindow) {
    use slint::platform::WindowEvent as SlintEvent;
    use slint::winit_030::winit::event::{ElementState, WindowEvent};
    use slint::winit_030::winit::keyboard::{Key, ModifiersState, PhysicalKey};
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    let ui_weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        let ui_weak = ui.as_weak();
        let mut mods = ModifiersState::empty();
        let drop_ui = ui.as_weak();
        ui.window().on_winit_window_event(move |window, event| {
            match event {
                // A settings file dragged onto the window: import it.
                WindowEvent::DroppedFile(path) => {
                    if let Some(ui) = drop_ui.upgrade() {
                        ui.invoke_import_file(path.to_string_lossy().to_string().into());
                    }
                }
                WindowEvent::ModifiersChanged(m) => mods = m.state(),
                WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                    if let (Key::Character(typed), PhysicalKey::Code(code)) = (&event.logical_key, event.physical_key)
                        && let Some(text) = latin_shortcut(typed, code, mods)
                    {
                        let text = slint::SharedString::from(text.to_string());
                        let _ = window.dispatch_event_with_result(match event.state {
                            ElementState::Pressed if event.repeat => SlintEvent::KeyPressRepeated { text },
                            ElementState::Pressed => SlintEvent::KeyPressed { text },
                            ElementState::Released => SlintEvent::KeyReleased { text },
                        });
                        return EventResult::PreventDefault;
                    }
                }
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                WindowEvent::Focused(true) | WindowEvent::Occluded(false) | WindowEvent::Resized(_) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_repaint_all(!ui.get_repaint_all());
                    }
                }
                _ => {}
            }
            EventResult::Propagate
        });
    });
}

/// The Latin letter to report for a Ctrl/Cmd shortcut that typed a non-Latin
/// character, e.g. "v" for Ctrl+"м" on the V key. None for anything else
/// (Latin layouts, AltGr, keys outside A–Z).
fn latin_shortcut(
    typed: &str,
    code: slint::winit_030::winit::keyboard::KeyCode,
    mods: slint::winit_030::winit::keyboard::ModifiersState,
) -> Option<char> {
    use slint::winit_030::winit::keyboard::KeyCode::*;
    const LETTERS: [slint::winit_030::winit::keyboard::KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR,
        KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    if !(mods.control_key() || mods.super_key()) || mods.alt_key() || typed.is_ascii() {
        return None;
    }
    let letter = (b'a' + LETTERS.iter().position(|k| *k == code)? as u8) as char;
    Some(if mods.shift_key() { letter.to_ascii_uppercase() } else { letter })
}

/// Routing changed while connected: restart xray with the new rules and keep
/// the tunnel, or reconnect when the tunnel itself has to change.
fn apply_routing(ui: &AppWindow, app: &Shared) {
    let geo = routing_ui::geo_dir(app);
    let mut st = app.lock().unwrap();
    let (ConnState::Connected { .. }, Some(conn), false) = (&st.conn_state, &st.conn, st.xray_restarting) else {
        return;
    };
    let Some(job) = conn.rerouter(&st.store.settings.routing, &geo) else {
        drop(st);
        diag::log("routing: apps changed, reconnecting");
        return reconnect(ui, app);
    };
    let generation = st.conn_gen;
    st.xray_restarting = true;
    drop(st);
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let result = job();
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.xray_restarting = false;
            if st.conn_gen != generation {
                return;
            }
            match result {
                Ok(Some(warnings)) => {
                    st.xray_started = Some(std::time::Instant::now());
                    diag::log("routing applied without reconnecting");
                    ui.set_routing_warnings(
                        if warnings.is_empty() { String::new() } else { format!("Предупреждения:\n{}", warnings.join("\n")) }.into(),
                    );
                    flash_status(&ui, &app, &mut st, "Маршрутизация применена".into());
                    render(&ui, &st);
                }
                other => {
                    if let Err(e) = &other {
                        diag::log(format!("routing: in-place apply failed ({}), reconnecting", first_line(&format!("{e:#}"))));
                    }
                    drop(st);
                    reconnect(&ui, &app);
                }
            }
        });
    });
}

fn switch_connection(ui: &AppWindow, app: &Shared, force: bool) {
    let mut st = app.lock().unwrap();
    let Some(wanted) = server_key(ui, &st) else { return };
    let ConnState::Connected { key, .. } = &st.conn_state else { return };
    if *key == wanted && !force {
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

/// "Подключаться при запуске": the last server, a moment after start. Right
/// after login the network may not be up yet: each try first waits for a
/// default route (up to two minutes), and two more tries follow 10 s apart.
fn autoconnect(ui: &AppWindow, app: &Shared, attempt: u32) {
    const NETWORK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
    let delay = if attempt == 0 { 1500 } else { 10_000 };
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    slint::Timer::single_shot(std::time::Duration::from_millis(delay), move || {
        // Off the UI thread: finding the default route runs a command.
        std::thread::spawn(move || {
            let (start, mut waiting) = (std::time::Instant::now(), false);
            while connection::uplink_interface().is_none() && start.elapsed() < NETWORK_WAIT {
                if !waiting {
                    diag::log("autoconnect: ждём сеть");
                    waiting = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            let _ = ui_weak.upgrade_in_event_loop(move |ui| autoconnect_now(&ui, &app, attempt));
        });
    });
}

fn autoconnect_now(ui: &AppWindow, app: &Shared, attempt: u32) {
    let ready = {
        let st = app.lock().unwrap();
        matches!(st.conn_state, ConnState::Idle | ConnState::Failed(_)) && selected_server(ui, &st).is_some()
    };
    if !ready {
        return;
    }
    diag::log(format!("autoconnect (try {})", attempt + 1));
    start_connect(ui, app);
    if attempt < 2 {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        slint::Timer::single_shot(std::time::Duration::from_secs(15), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if matches!(app.lock().unwrap().conn_state, ConnState::Failed(_)) {
                autoconnect(&ui, &app, attempt + 1);
            }
        });
    }
}

fn toggle_connection(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    match st.conn_state {
        ConnState::Connected { .. } | ConnState::Blocked => {
            let conn = st.conn.take();
            diag::log("disconnect");
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
        // "Отменить": a connect that hangs (xray not answering) can be stopped.
        ConnState::Connecting { .. } => {
            diag::log("connect cancelled");
            st.conn_gen += 1;
            st.conn_state = ConnState::Idle;
            // The attempt stops at its next step (the wait for xray at once)
            // and takes down whatever it started before the next one runs.
            st.conn_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            render(ui, &st);
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
    // Single servers of the same subscription, to name a balancer's nodes.
    let siblings = current(ui, &st).map(|s| s.servers.clone()).unwrap_or_default();
    diag::log(format!("connect: {} {}/{}, {} outbound(s)", server.protocol, server.network, server.security, server.proxies));
    st.conn_gen += 1;
    let generation = st.conn_gen;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    st.conn_cancel = cancel.clone();
    st.conn_state = ConnState::Connecting { name: name.clone() };
    let run_dir = st.path.parent().map(|p| p.join("run")).unwrap_or_else(|| PathBuf::from("run"));
    let routing = st.store.settings.routing.clone();
    let proxy_port = st.store.settings.proxy_mode.then_some(st.store.settings.proxy_port);
    let kill_switch = st.store.settings.kill_switch;
    render(ui, &st);
    drop(st);
    let geo = routing_ui::geo_dir(app);

    let on_failure = {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move |msg: String, blocked: bool| {
            let app = app.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| connection_failed(&ui, &app, generation, msg, blocked));
        }
    };
    let on_uplink_change = {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let app = app.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let current = {
                    let st = app.lock().unwrap();
                    st.conn_gen == generation && matches!(st.conn_state, ConnState::Connected { .. })
                };
                if current {
                    reconnect(&ui, &app);
                }
            });
        }
    };
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let result = connection::connect(
            &server,
            &run_dir,
            &routing,
            &geo,
            connection::Options { proxy_port, kill_switch, cancel },
            on_failure,
            on_uplink_change,
        );
        let node_names = match &result {
            Ok(conn) if conn.balancer.is_some() => duoray_core::balancer::node_names(&server.config_json(), &siblings),
            _ => vec![],
        };
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            let still_wanted = st.conn_gen == generation && matches!(st.conn_state, ConnState::Connecting { .. });
            // Cancelled meanwhile: drop whatever came back (a connection that
            // made it is taken down below), and do not report the abort.
            if !still_wanted && result.is_err() {
                return;
            }
            match result {
                Ok(conn) if still_wanted => {
                    ui.set_routing_warnings(
                        if conn.warnings.is_empty() {
                            String::new()
                        } else {
                            format!("Предупреждения:\n{}", conn.warnings.join("\n"))
                        }
                        .into(),
                    );
                    diag::log(format!("connected via {} {}", conn.tun, conn.timings));
                    st.conn_state = ConnState::Connected { name, tun: conn.tun.clone(), key };
                    st.conn = Some(conn);
                    st.xray_started = Some(std::time::Instant::now());
                    st.live_ping = None;
                    st.live_failures = 0;
                    (st.exit, st.exit_node, st.traffic) = (None, None, None);
                    st.node_names = node_names.into_iter().collect();
                    // First reading right away, then every 5 s from the timer.
                    measure_live_ping(&ui, &app, &mut st);
                    measure_exit(&ui, &app, &mut st);
                }
                Ok(conn) => {
                    std::thread::spawn(move || conn.disconnect());
                }
                Err(ConnectError::HelperMissing) => {
                    diag::log("helper missing");
                    st.conn_state = ConnState::Idle;
                    st.connect_after_install = true;
                    ui.set_helper_error("".into());
                    ui.set_helper_dialog_text(
                        if proxy_port.is_some() {
                            "Системный прокси в macOS меняется только с правами администратора, поэтому DUORAY нужен \
                             небольшой системный помощник. Он устанавливается один раз: система попросит пароль \
                             администратора. Дальше подключение работает без пароля."
                        } else {
                            "Для VPN-туннеля (TUN) DUORAY нужен небольшой системный помощник. Он устанавливается один раз: \
                             система попросит пароль администратора. Дальше подключение работает без пароля."
                        }
                        .into(),
                    );
                    ui.set_helper_dialog(true);
                }
                Err(ConnectError::HelperOutdated(v)) => {
                    diag::log(format!("helper outdated: {v}"));
                    st.conn_state = ConnState::Idle;
                    st.connect_after_install = true;
                    ui.set_helper_error("".into());
                    ui.set_helper_dialog_text(
                        format!("Установлена старая версия помощника ({v}). Обновите её, понадобится пароль администратора.")
                            .into(),
                    );
                    ui.set_helper_dialog(true);
                }
                Err(ConnectError::Other(e)) => {
                    diag::auto_online("connect_failed", &format!("{e:#}"));
                    st.conn_state = ConnState::Failed(humanize(&format!("{e:#}")));
                }
            }
            render(&ui, &st);
            drop(st);
            // The user may have picked another server while we were connecting.
            follow_selection(&ui, &app);
        });
    });
}

fn connection_failed(ui: &AppWindow, app: &Shared, generation: u64, msg: String, blocked: bool) {
    let mut st = app.lock().unwrap();
    let key = match &st.conn_state {
        ConnState::Connected { key, .. } if st.conn_gen == generation => key.clone(),
        _ => return,
    };
    diag::auto("tunnel_stopped", if msg.is_empty() { "helper disconnected" } else { &msg });
    // Kill switch: the tunnel stays up (and blocks) until "Отключиться".
    if blocked {
        st.conn_state = ConnState::Blocked;
        render(ui, &st);
        return;
    }
    let conn = st.conn.take();
    st.conn_state = ConnState::Failed(if msg.is_empty() { "Помощник отключился".into() } else { msg });
    // "Держать подключение": the same server again, once the network is there.
    let keep = st.store.settings.keep_connected;
    if keep {
        diag::log("keep connected: туннель упал, переподключаемся");
        st.store.settings.last_server = Some(key);
        restore_selection(ui, &st);
    }
    render(ui, &st);
    drop(st);
    if let Some(c) = conn {
        std::thread::spawn(move || c.disconnect());
    }
    if keep {
        autoconnect(ui, app, 0);
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

/// The Happ fields of the subscription identity dialog.
fn load_sub_happ(w: &AppWindow, h: &HappSpoof) {
    w.set_ih_version(h.app_version.clone().into());
    w.set_ih_os(h.os.clone().into());
    w.set_ih_os_version(h.os_version.clone().into());
    w.set_ih_model(h.model.clone().into());
    w.set_ih_locale(h.locale.clone().into());
    w.set_ih_user_id(h.user_id.clone().into());
    w.set_ih_hwid(h.hwid.clone().into());
}

fn read_sub_happ(w: &AppWindow) -> HappSpoof {
    HappSpoof {
        enabled: true,
        app_version: w.get_ih_version().trim().to_string(),
        os: w.get_ih_os().trim().to_string(),
        os_version: w.get_ih_os_version().trim().to_string(),
        model: w.get_ih_model().trim().to_string(),
        locale: w.get_ih_locale().trim().to_string(),
        user_id: w.get_ih_user_id().trim().to_string(),
        hwid: w.get_ih_hwid().trim().to_string(),
    }
}

fn current<'a>(ui: &AppWindow, st: &'a App) -> Option<&'a Subscription> {
    usize::try_from(ui.get_current_sub()).ok().and_then(|i| st.store.subscriptions.get(i))
}

fn save(st: &mut App) {
    if std::env::var_os("DUORAY_SNAPSHOT").is_some() {
        return;
    }
    if let Err(e) = st.store.save(&st.path) {
        st.status = format!("Не удалось сохранить: {e:#}");
    }
}

fn refresh(ui: &AppWindow, app: &Shared, device: &Arc<Device>, id: String) {
    let (url, fallback, headers, proxy) = {
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
        let own = sub.identity.clone();
        let proxy = st.conn.as_ref().map(|c| match c.user.as_str() {
            // Proxy mode: no password.
            "" => format!("socks5h://{}", c.socks),
            user => format!("socks5h://{user}:{}@{}", c.pass, c.socks),
        });
        let settings = &st.store.settings;
        (url, fallback, identity::subscription_headers(&settings.happ, device, settings.send_device_info, &own), proxy)
    };
    render(ui, &app.lock().unwrap());

    let (ui_weak, app): (Weak<AppWindow>, Shared) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let result = subscription::fetch(&url, fallback.as_deref(), &headers)
            .or_else(|original| match proxy.as_deref() {
                Some(proxy) => subscription::fetch_with_proxy(&url, fallback.as_deref(), &headers, Some(proxy)).map_err(|_| original),
                None => Err(original),
            })
            .map_err(|e| humanize(&format!("{e:#}")));
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.refreshing.remove(&id);
            match &result {
                Ok(f) => {
                    st.fetch_failures.remove(&id);
                    diag::log(format!("subscription updated: {} servers, {} skipped", f.servers.len(), f.skipped.len()));
                }
                Err(e) => {
                    let failures = st.fetch_failures.entry(id.clone()).or_default();
                    *failures += 1;
                    if *failures >= 2 {
                        diag::auto_online("subscription", e);
                    } else {
                        diag::log(format!("subscription: {} (will retry)", first_line(e)));
                    }
                }
            }
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
    diag::set_context(diag_context(ui, st));
    let manual = st.store.subscriptions.iter().find(|s| s.is_manual());
    ui.set_manual_meta(manual.map_or_else(|| "Нет серверов".to_string(), sub_meta).into());
    ui.set_manual_selected(current(ui, st).is_some_and(Subscription::is_manual));
    ui.set_has_manual(manual.is_some_and(|m| !m.servers.is_empty()));
    let subs: Vec<SubRow> = st
        .store
        .subscriptions
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.is_manual())
        .map(|(index, s)| SubRow {
            index: index as i32,
            name: s.display_name().into(),
            name_parts: emoji::name_parts(&s.display_name()),
            meta: sub_meta(s).into(),
            usage: usage(s),
            error: s.last_error.as_deref().map(first_line).unwrap_or_default().into(),
        })
        .collect();
    update_model(ui.get_subs(), subs, |m| ui.set_subs(m));

    let busy_count = st.refreshing.len();
    ui.set_status(
        if let Some((done, total)) = st.ping_progress {
            format!("Пинг: {done} из {total}")
        } else if let Some((done, total)) = st.speed_progress {
            let live = st.speed_live.as_ref().map(|l| format!(" · {}", l.label())).unwrap_or_default();
            format!("Тест скорости {} из {total}{live}", (done + 1).min(total))
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
        ConnState::Blocked => (
            "blocked",
            "Интернет заблокирован".into(),
            "Сервер недоступен. Трафик остановлен, чтобы не идти мимо VPN".into(),
        ),
    };
    ui.set_conn_state(state.into());
    ui.set_conn_title(title.into());
    ui.set_conn_detail(detail.into());
    tray::update(tray_state(ui, st));
    let (live_text, live_level) = match (&st.conn_state, &st.live_ping) {
        (ConnState::Connected { .. }, Some(p)) => p.clone(),
        (ConnState::Connected { .. }, None) => ("…".to_string(), 5),
        _ => (String::new(), 0),
    };
    ui.set_conn_ping_text(live_text.into());
    ui.set_conn_ping_level(live_level);
    let connected = matches!(st.conn_state, ConnState::Connected { .. });
    let exit = st.exit.as_ref().filter(|_| connected);
    // The tile is 42 px wide (times the text size), in physical pixels here.
    let tile = (42.0 * ui.window().scale_factor() * st.store.settings.text_scale.clamp(80, 150) as f32 / 100.0).round() as u32;
    ui.set_conn_exit_flag(exit.and_then(|t| flag_tile(&t.loc, tile)).unwrap_or_default());
    ui.set_conn_server(match &st.conn_state {
        ConnState::Connected { name, .. } => name.clone().into(),
        _ => "".into(),
    });
    ui.set_conn_exit_text(
        exit.map(|t| duoray_core::balancer::country_name(&t.loc).unwrap_or(&t.loc).to_string()).unwrap_or_default().into(),
    );
    ui.set_conn_node(st.exit_node.clone().filter(|_| connected).unwrap_or_default().into());
    ui.set_speed_testing(st.speed_cancel.is_some());
    ui.set_conn_traffic(st.traffic.as_ref().filter(|_| connected).map(Traffic::label).unwrap_or_default().into());
    ui.set_can_connect(selected_server(ui, st).is_some());
    render_expiry(ui, st);

    let Some(sub) = current(ui, st) else {
        update_model(ui.get_servers(), Vec::new(), |m| ui.set_servers(m));
        return;
    };
    ui.set_header_title(sub.display_name().into());
    ui.set_header_title_parts(emoji::name_parts(&sub.display_name()));
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
            let speed = SpeedCell::row(st.speed_results.get(&key_of(sub, s)));
            ServerRow {
                has_flag_image: image.is_some(),
                flag_image: image.unwrap_or_default(),
                flag: flag.unwrap_or_default().into(),
                name: name.into(),
                name_parts: emoji::name_parts(emoji::without_leading_flag(&s.name)),
                description: s.description.clone().unwrap_or_default().into(),
                protocol: s.protocol.clone().into(),
                transport: transport(&s.network, &s.security).into(),
                endpoint: format!("{}:{}", s.address, s.port).into(),
                proxies: s.proxies as i32,
                ping_text: ping.0.into(),
                ping_level: ping.1,
                speed_state: speed.0,
                speed_down: speed.1.into(),
                speed_up: speed.2.into(),
                edited: s.override_key.is_some(),
                favorite: st.store.settings.favorites.contains(&key_of(sub, s)),
                is_json: matches!(s.source, duoray_core::server::Source::Json(_)),
            }
        })
        .collect();
    update_model(ui.get_servers(), rows, |m| ui.set_servers(m));
}

/// Puts `rows` into the list model in place, touching only rows that
/// changed: the list keeps its elements, so a context menu open on a row
/// survives the redraws of a ping or a speed test. A new model only the
/// first time.
fn update_model<T: Clone + PartialEq + 'static>(model: ModelRc<T>, rows: Vec<T>, set: impl FnOnce(ModelRc<T>)) {
    use slint::Model;
    let Some(vec) = model.as_any().downcast_ref::<VecModel<T>>() else {
        return set(ModelRc::new(VecModel::from(rows)));
    };
    if vec.row_count() != rows.len() {
        return vec.set_vec(rows);
    }
    for (i, row) in rows.into_iter().enumerate() {
        if vec.row_data(i).as_ref() != Some(&row) {
            vec.set_row_data(i, row);
        }
    }
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
/// Opens a local file with its default app (a downloaded .dmg, say).
fn import_file(ui: &AppWindow, app: &Shared, device: &Arc<Device>, path: &std::path::Path) {
    match std::fs::read_to_string(path) {
        Ok(text) => import_settings(ui, app, device, &text),
        Err(e) => {
            let mut st = app.lock().unwrap();
            flash_status_for(ui, app, &mut st, format!("Не удалось открыть файл: {e}"), 8);
            render(ui, &st);
        }
    }
}

/// Applies an exported settings file (see `Store::import`). The store as it
/// was stays next to it as store.json.bak.
fn import_settings(ui: &AppWindow, app: &Shared, device: &Arc<Device>, text: &str) {
    let mut st = app.lock().unwrap();
    let backup = st.path.with_extension("json.bak");
    let _ = std::fs::copy(&st.path, backup);
    match st.store.import(text) {
        Ok((added, servers)) => {
            save(&mut st);
            diag::log(format!("settings imported: {} subscriptions, {servers} servers", added.len()));
            apply_appearance(ui, &st.store.settings);
            let msg = format!(
                "Настройки перенесены. Добавлено: {}, {}",
                plural(added.len(), "подписка", "подписки", "подписок"),
                plural(servers, "сервер", "сервера", "серверов")
            );
            flash_status_for(ui, app, &mut st, msg, 8);
            render(ui, &st);
            drop(st);
            // The settings page shows what was imported.
            ui.invoke_open_settings();
            for id in added {
                refresh(ui, app, device, id);
            }
        }
        Err(e) => {
            flash_status_for(ui, app, &mut st, format!("Импорт не удался: {e:#}"), 8);
            render(ui, &st);
        }
    }
}

/// The user's Downloads folder (XDG's, localized, on Linux), else home.
fn downloads_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map_or_else(|| PathBuf::from("."), PathBuf::from);
    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("xdg-user-dir").arg("DOWNLOAD").output() {
        let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if dir.is_dir() && dir != home {
            return dir;
        }
    }
    let dir = home.join("Downloads");
    if dir.is_dir() { dir } else { home }
}

fn open_path(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("/usr/bin/open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(windows)]
    let _ = connection::hidden(std::process::Command::new("explorer").arg(path)).spawn();
}

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

fn is_dualizm(sub: &Subscription) -> bool {
    let host = url::Url::parse(&sub.url).ok().and_then(|u| u.host_str().map(str::to_lowercase)).unwrap_or_default();
    host == "dualizm.space"
        || host.ends_with(".dualizm.space")
        || sub.info.title.as_deref().is_some_and(|t| t.to_uppercase().contains("DUALIZM"))
}

/// A DUALIZM subscription that still works: has servers and has not expired.
fn is_active_dualizm(sub: &Subscription) -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    is_dualizm(sub) && !sub.servers.is_empty() && sub.info.expire.is_none_or(|e| e == 0 || e > now)
}

/// The reminder starts this many days before the end and lasts a week after.
const EXPIRY_AHEAD: u64 = 3 * 86400;
const EXPIRY_AFTER: u64 = 7 * 86400;

/// The subscription closest to its end, unless its reminder was closed.
fn expiring(subs: &[Subscription], now: u64) -> Option<&Subscription> {
    subs.iter()
        .filter_map(|s| s.info.expire.filter(|&e| e > 0 && s.expiry_dismissed != Some(e)).map(|e| (s, e)))
        .filter(|&(_, e)| e < now + EXPIRY_AHEAD && e + EXPIRY_AFTER > now)
        .min_by_key(|&(_, e)| e)
        .map(|(s, _)| s)
}

/// "Подписка «DUALIZM» заканчивается завтра в 14:00"
fn expiry_text(name: &str, expire: u64, now: u64) -> String {
    use chrono::TimeZone;
    if expire <= now {
        return format!("Подписка «{name}» закончилась");
    }
    let local = |t: u64| chrono::Local.timestamp_opt(t as i64, 0).single();
    let (Some(end), Some(today)) = (local(expire), local(now)) else {
        return format!("Подписка «{name}» заканчивается {}", date(expire));
    };
    let when = match (end.date_naive() - today.date_naive()).num_days() {
        0 => format!("сегодня в {}", end.format("%H:%M")),
        1 => format!("завтра в {}", end.format("%H:%M")),
        n => format!("через {} ({})", plural(n as usize, "день", "дня", "дней"), end.format("%d.%m")),
    };
    format!("Подписка «{name}» заканчивается {when}")
}

/// Where to renew: the DUALIZM bot, otherwise the panel's page or support.
fn renew_url(sub: &Subscription) -> Option<String> {
    if is_dualizm(sub) {
        return Some(BUY_URL.to_string());
    }
    sub.info.web_page_url.clone().or_else(|| sub.info.support_url.clone())
}

fn render_expiry(ui: &AppWindow, st: &App) {
    let sub = expiring(&st.store.subscriptions, store::now());
    let text = sub.and_then(|s| Some(expiry_text(&emoji::strip(&s.display_name()), s.info.expire?, store::now())));
    ui.set_expiry_banner(text.unwrap_or_default().into());
    ui.set_expiry_can_renew(sub.and_then(renew_url).is_some());
    // The reminder's "Продлить" already leads to the bot.
    let renewing = sub.is_some_and(is_dualizm);
    ui.set_show_buy(
        !st.store.settings.hide_buy && !renewing && !st.store.subscriptions.iter().any(is_active_dualizm),
    );
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
        // The helper runs as SYSTEM: only security software refuses it a program.
        (
            "spawning netsh",
            "Windows не дал запустить netsh: похоже, мешает антивирус. Добавьте папку DUORAY в его исключения и подключитесь снова.",
        ),
    ];
    if let Some((_, app)) = e.split_once("the panel redirects to the ") {
        let app = app.split(' ').next().unwrap_or_default();
        return format!(
            "Панель отдала ссылку для приложения {app}, а не подписку. Скопируйте ссылку на подписку (https://…) в личном кабинете."
        );
    }
    if let Some((_, note)) = e.split_once("a note instead of servers: ") {
        return format!("Серверов нет, панель пишет: «{note}»");
    }
    for (needle, text) in map {
        if e.contains(needle) {
            return text.to_string();
        }
    }
    e.to_string()
}

const PING_THREADS: [u32; 5] = [1, 2, 4, 8, 16];

fn load_ping(ui: &AppWindow, p: &duoray_core::store::PingSettings) {
    use duoray_core::store::{PingMode, ProxyPingMode};
    ui.set_ping_mode_index(match p.mode {
        PingMode::HttpGet => 0,
        PingMode::HttpHead => 1,
        PingMode::Tcp => 2,
        PingMode::Icmp => 3,
    });
    ui.set_ping_proxy_mode(match p.proxy_mode {
        ProxyPingMode::Default => 0,
        ProxyPingMode::Double => 1,
        ProxyPingMode::KeepAlive => 2,
    });
    let threads = PING_THREADS.iter().copied().filter(|t| *t <= p.threads).max().unwrap_or(1);
    ui.set_ping_threads(threads.to_string().into());
    ui.set_ping_url(p.url.clone().into());
}

fn read_ping(ui: &AppWindow, old: &duoray_core::store::PingSettings) -> duoray_core::store::PingSettings {
    use duoray_core::store::{PingMode, ProxyPingMode};
    let url = ui.get_ping_url().trim().to_string();
    duoray_core::store::PingSettings {
        mode: match ui.get_ping_mode_index() {
            1 => PingMode::HttpHead,
            2 => PingMode::Tcp,
            3 => PingMode::Icmp,
            _ => PingMode::HttpGet,
        },
        proxy_mode: match ui.get_ping_proxy_mode() {
            1 => ProxyPingMode::Double,
            2 => ProxyPingMode::KeepAlive,
            _ => ProxyPingMode::Default,
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
    // An own file needs a URL; without one the test falls back to Ookla.
    let speed_url = ui.get_speedtest_url().trim().to_string();
    let is_url = speed_url.starts_with("http://") || speed_url.starts_with("https://");
    st.store.settings.speedtest = match ui.get_speedtest_source() {
        1 => SpeedSource::Cloudflare,
        2 if is_url => SpeedSource::Own,
        _ => SpeedSource::Ookla,
    };
    st.store.settings.speedtest_url = if is_url { speed_url } else { String::new() };
    st.store.settings.send_device_info = ui.get_send_device_info();
    st.store.settings.hysteria_restart = ui.get_hysteria_restart();
    st.store.settings.hysteria_restart_minutes = ui.get_hysteria_minutes().parse().unwrap_or(5);
    st.store.settings.auto_update = ui.get_auto_update();
    st.store.settings.switch_on_select = ui.get_switch_on_select();
    st.store.settings.keep_connected = ui.get_keep_connected();
    st.store.settings.show_server_type = ui.get_show_server_type();
    st.store.settings.server_filter_enabled = ui.get_filter_enabled();
    // Off: the list shows every server; the pattern is kept for later.
    let s = &st.store.settings;
    st.filter = active_filter(s.server_filter_enabled, &s.server_filter, s.server_filter_exclude);
    st.store.settings.close_to_tray = ui.get_close_to_tray();
    st.store.settings.autoconnect = ui.get_autoconnect();
    st.store.settings.kill_switch = ui.get_kill_switch();
    if ui.get_autostart() != st.store.settings.autostart {
        st.store.settings.autostart = ui.get_autostart();
        if let Err(e) = autostart::set(st.store.settings.autostart) {
            diag::log(format!("autostart: {e:#}"));
            st.status = format!("Не удалось настроить автозапуск: {e:#}");
        }
    }
    let mode = (st.store.settings.proxy_mode, st.store.settings.proxy_port);
    st.store.settings.proxy_mode = ui.get_proxy_mode();
    if let Some(port) = ui.get_proxy_port().trim().parse::<u16>().ok().filter(|&p| p > 0) {
        st.store.settings.proxy_port = port;
    }
    // TUN ↔ proxy (or another port) while connected: connect the new way.
    let mode_changed = mode != (st.store.settings.proxy_mode, st.store.settings.proxy_port)
        && matches!(st.conn_state, ConnState::Connected { .. });
    let reports = ui.get_telemetry_enabled();
    if st.store.settings.telemetry.is_some_and(|t| t != reports) {
        support_ui::set_consent(&mut st.store.settings, reports);
    }
    let theme = ui.get_theme_choice();
    st.store.settings.theme = THEMES.iter().find(|t| t.1 == theme.as_str()).map_or("dark", |t| t.0).to_string();
    st.store.settings.text_scale = ui.get_text_scale_choice().trim_end_matches('%').parse().unwrap_or(100);
    let font = ui.get_font_choice().to_string();
    st.store.settings.font = if font == SYSTEM_FONT { String::new() } else { font };
    apply_appearance(ui, &st.store.settings);
    save(&mut st);
    // The filter switch changes which servers the list shows.
    render(ui, &st);
    drop(st);
    if mode_changed {
        reconnect(ui, app);
    }
}

const SYSTEM_FONT: &str = "Системный";

/// Common fonts that ship with each OS (nothing is bundled).
#[cfg(target_os = "macos")]
const FONTS: &[&str] = &["Helvetica Neue", "Avenir Next", "Futura", "Gill Sans", "Menlo"];
#[cfg(windows)]
const FONTS: &[&str] = &["Segoe UI", "Arial", "Verdana", "Tahoma", "Consolas"];
#[cfg(not(any(target_os = "macos", windows)))]
const FONTS: &[&str] = &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Ubuntu", "DejaVu Sans Mono"];

/// Colors of a palette theme, as RGB: main area, sidebar and cards, text,
/// borders, accent and the text on it, selected/secondary surfaces and the
/// text on them, switch tracks.
struct Palette {
    light: bool,
    bg: u32,
    card: u32,
    fg: u32,
    line: u32,
    accent: u32,
    on_accent: u32,
    tonal: u32,
    on_tonal: u32,
    track: u32,
    /// Hairline around cards and along panel edges, for themes whose cards
    /// barely differ from the background (OLED).
    outline: Option<u32>,
}

/// A dark palette, colors in the field order above.
const fn dark(c: [u32; 9]) -> Palette {
    let [bg, card, fg, line, accent, on_accent, tonal, on_tonal, track] = c;
    Palette { light: false, bg, card, fg, line, accent, on_accent, tonal, on_tonal, track, outline: None }
}

/// id (stored), name (shown), Slint color scheme (0 system, 1 dark, 2 light),
/// palette (None: Slint's Material colors with the DUORAY accent).
/// The palette themes take their colors from the projects' own palettes.
const THEMES: &[(&str, &str, i32, Option<Palette>)] = &[
    ("system", "Как в системе", 0, None),
    ("dark", "Тёмная", 1, None),
    ("light", "Светлая", 2, None),
    // Neutral greys a step lighter than Material's violet-tinted dark.
    ("graphite", "Графитовая", 1, Some(dark([0x2f3136, 0x25272b, 0xe4e5e8, 0x8f9197, 0xd0bcff, 0x381e72, 0x45474d, 0xe4e5e8, 0x3a3c41]))),
    // True black: OLED pixels switch off.
    (
        "oled",
        "OLED (чёрная)",
        1,
        Some(Palette {
            outline: Some(0x2c2c30),
            ..dark([0x000000, 0x0a0a0b, 0xe6e6e6, 0x5a5a5e, 0xd0bcff, 0x381e72, 0x2a2a2e, 0xe6e6e6, 0x26262a])
        }),
    ),
    // Catppuccin flavors, lightest dark to darkest: base, mantle, text, overlay0, mauve, surface1, surface0.
    ("catppuccin-frappe", "Catppuccin Frappé", 1, Some(dark([0x303446, 0x292c3c, 0xc6d0f5, 0x737994, 0xca9ee6, 0x303446, 0x51576d, 0xc6d0f5, 0x414559]))),
    ("catppuccin-macchiato", "Catppuccin Macchiato", 1, Some(dark([0x24273a, 0x1e2030, 0xcad3f5, 0x6e738d, 0xc6a0f6, 0x24273a, 0x494d64, 0xcad3f5, 0x363a4f]))),
    ("catppuccin", "Catppuccin Mocha", 1, Some(dark([0x1e1e2e, 0x181825, 0xcdd6f4, 0x6c7086, 0xcba6f7, 0x1e1e2e, 0x45475a, 0xcdd6f4, 0x313244]))),
    (
        "catppuccin-latte",
        "Catppuccin Latte",
        2,
        Some(Palette { light: true, bg: 0xeff1f5, card: 0xe6e9ef, fg: 0x4c4f69, line: 0x9ca0b0, accent: 0x8839ef, on_accent: 0xffffff, tonal: 0xccd0da, on_tonal: 0x4c4f69, track: 0xdce0e8, outline: None }),
    ),
    // From a user's palette (#FFF5F5, #F7D6D0, #E2B4BD, #4A4A4A): near-white
    // warm surfaces, the peach only where something is selected, and the
    // coral deepened so icons and white button text keep their contrast.
    (
        "coral",
        "Коралловая",
        2,
        Some(Palette { light: true, bg: 0xfffaf9, card: 0xfbeeeb, fg: 0x3d3838, line: 0xc9a6ac, accent: 0xc75545, on_accent: 0xffffff, tonal: 0xf7d6d0, on_tonal: 0x3d3838, track: 0xecd8d6, outline: None }),
    ),
    // Its dark pair: warm plum-brown surfaces, a light coral accent.
    ("coral-dark", "Тёмно-коралловая", 1, Some(dark([0x2a2022, 0x221a1c, 0xf2e4e2, 0x8c6f72, 0xf08a76, 0x3a1510, 0x4a3336, 0xf2e4e2, 0x3b2c2f]))),
    ("tokyo-night", "Tokyo Night", 1, Some(dark([0x1a1b26, 0x16161e, 0xc0caf5, 0x565f89, 0x7aa2f7, 0x1a1b26, 0x343a55, 0xc0caf5, 0x292e42]))),
    ("dracula", "Dracula", 1, Some(dark([0x282a36, 0x21222c, 0xf8f8f2, 0x6272a4, 0xbd93f9, 0x282a36, 0x44475a, 0xf8f8f2, 0x343746]))),
    ("nord", "Nord", 1, Some(dark([0x2e3440, 0x272c36, 0xeceff4, 0x616e88, 0x88c0d0, 0x2e3440, 0x434c5e, 0xeceff4, 0x3b4252]))),
    ("one-dark", "One Dark", 1, Some(dark([0x282c34, 0x21252b, 0xd7dae0, 0x5c6370, 0x61afef, 0x282c34, 0x3e4451, 0xd7dae0, 0x2c313a]))),
    ("gruvbox", "Gruvbox", 1, Some(dark([0x282828, 0x1d2021, 0xebdbb2, 0x928374, 0xfabd2f, 0x282828, 0x504945, 0xebdbb2, 0x3c3836]))),
    ("rose-pine", "Rosé Pine", 1, Some(dark([0x1f1d2e, 0x191724, 0xe0def4, 0x6e6a86, 0xc4a7e7, 0x191724, 0x403d52, 0xe0def4, 0x26233a]))),
    // Green-black with soft phosphor text; the bright green only on accents.
    (
        "matrix",
        "Матрица",
        1,
        Some(Palette {
            outline: Some(0x15402a),
            ..dark([0x06100a, 0x030805, 0xc4f5cf, 0x2a6b3a, 0x39e66b, 0x022110, 0x10301a, 0xd2fadb, 0x0d2414])
        }),
    ),
];

/// Material's own colors, for the tiles of the themes without a palette.
const MATERIAL_DARK: [u32; 4] = [0x2a282d, 0x1c1b1f, 0xe6e1e5, 0xd0bcff];
const MATERIAL_LIGHT: [u32; 4] = [0xf8f3f9, 0xece6f0, 0x1d1b20, 0x6750a4];

/// The tile Settings shows for a theme.
fn theme_swatch(t: &(&str, &str, i32, Option<Palette>)) -> ThemeSwatch {
    let c = |rgb: u32| slint::Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    let (first, second) = match (&t.3, t.2) {
        (Some(p), _) => ([p.bg, p.card, p.fg, p.accent], None),
        (None, 0) => (MATERIAL_LIGHT, Some(MATERIAL_DARK)),
        (None, 2) => (MATERIAL_LIGHT, None),
        (None, _) => (MATERIAL_DARK, None),
    };
    let other = second.unwrap_or(first);
    ThemeSwatch {
        name: t.1.into(),
        bg: c(first[0]),
        card: c(first[1]),
        fg: c(first[2]),
        accent: c(first[3]),
        split: second.is_some(),
        bg2: c(other[0]),
        card2: c(other[1]),
        fg2: c(other[2]),
        accent2: c(other[3]),
    }
}

/// The theme with this id; an unknown one (from a newer version) is dark.
fn theme_of(id: &str) -> &'static (&'static str, &'static str, i32, Option<Palette>) {
    THEMES.iter().find(|t| t.0 == id).or_else(|| THEMES.iter().find(|t| t.0 == "dark")).unwrap()
}

fn apply_appearance(ui: &AppWindow, s: &duoray_core::store::Settings) {
    let theme = theme_of(&s.theme);
    ui.set_theme_mode(theme.2);
    let g = ui.global::<Theme>();
    g.set_custom(theme.3.is_some());
    if let Some(p) = &theme.3 {
        let c = |rgb: u32| slint::Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
        g.set_c_light(p.light);
        g.set_c_bg(c(p.bg));
        g.set_c_card(c(p.card));
        g.set_c_fg(c(p.fg));
        g.set_c_line(c(p.line));
        g.set_c_accent(c(p.accent));
        g.set_c_on_accent(c(p.on_accent));
        g.set_c_tonal(c(p.tonal));
        g.set_c_on_tonal(c(p.on_tonal));
        g.set_c_track(c(p.track));
        g.set_c_outline(p.outline.map_or(slint::Color::from_argb_u8(0, 0, 0, 0), c));
    }
    ui.global::<Theme>().set_text_scale(s.text_scale.clamp(80, 150) as f32 / 100.0);
    ui.set_font_family(s.font.clone().into());
    ui.set_show_server_type(s.show_server_type);
    ui.set_filter_enabled(s.server_filter_enabled);
}

fn restart_hysteria_if_due(ui: &AppWindow, app: &Shared) {
    let mut st = app.lock().unwrap();
    let settings = &st.store.settings;
    let every = std::time::Duration::from_secs(u64::from(settings.hysteria_restart_minutes.max(1)) * 60);
    let (true, false, Some(started), Some(conn), ConnState::Connected { .. }) =
        (settings.hysteria_restart, st.xray_restarting, st.xray_started, &st.conn, &st.conn_state)
    else {
        return;
    };
    if !conn.hysteria || started.elapsed() < every {
        return;
    }
    let job = conn.xray_restarter();
    let generation = st.conn_gen;
    st.xray_restarting = true;
    drop(st);
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let result = job();
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.xray_restarting = false;
            st.xray_started = Some(std::time::Instant::now());
            if st.conn_gen != generation {
                return;
            }
            let msg = match result {
                Ok(()) => "Hysteria2 перезапущена".to_string(),
                Err(e) => format!("Перезапуск Hysteria2 не удался: {}", first_line(&format!("{e:#}"))),
            };
            flash_status(&ui, &app, &mut st, msg);
            render(&ui, &st);
        });
    });
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
    let ping = st.store.settings.ping.clone();
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        // The same request kind and mode as the list's ping: the result lands in its row.
        let head = ping.mode == duoray_core::store::PingMode::HttpHead;
        let timeout = std::time::Duration::from_secs(5);
        let result = ping::via_socks_mode(socks, &user, &pass, &ping.url, head, timeout, ping.proxy_mode);
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.live_ping_busy = false;
            // Server switched or disconnected meanwhile: drop the stale reading.
            if st.conn_gen != generation || !matches!(st.conn_state, ConnState::Connected { .. }) {
                return;
            }
            let answered = result.is_ok();
            let entry = ping_entry(result.ok().map(|d| d.as_millis()));
            st.live_ping = Some((entry.0.clone(), entry.1));
            st.ping_results.insert(key, entry);
            save_pings(&st);
            if answered {
                st.live_failures = 0;
                st.failover_tried.clear();
            } else {
                st.live_failures += 1;
            }
            let unresponsive = st.live_failures >= LIVE_FAILURES && st.store.settings.keep_connected;
            render(&ui, &st);
            drop(st);
            if unresponsive {
                server_unresponsive(&ui, &app);
            }
        });
    });
}

/// Live pings in a row that a server may miss before "Держать подключение"
/// moves away from it: 15 s with the window shown, 30 s hidden.
const LIVE_FAILURES: u32 = 3;

/// "Держать подключение": the connected server has missed LIVE_FAILURES live
/// pings. With the network up, move to the best other server of the same
/// subscription; with it down, there is nothing to do but wait.
fn server_unresponsive(ui: &AppWindow, app: &Shared) {
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    // Off the UI thread: finding the default route runs a command.
    std::thread::spawn(move || {
        if connection::uplink_interface().is_none() {
            diag::log("keep connected: нет сети, ждём");
            return;
        }
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            // Answered meanwhile, or no longer connected.
            let ConnState::Connected { key, name, .. } = &st.conn_state else { return };
            if st.live_failures < LIVE_FAILURES {
                return;
            }
            let (key, name) = (key.clone(), name.clone());
            st.live_failures = 0;
            st.failover_tried.insert(key.clone());
            let Some((next, next_name)) = failover_target(&st, &key) else {
                // Every one tried: start over after the next misses.
                st.failover_tried.clear();
                diag::log(format!("keep connected: {name} не отвечает, другим некуда"));
                flash_status_for(&ui, &app, &mut st, "Серверы подписки не отвечают. Проверьте интернет".into(), 8);
                return render(&ui, &st);
            };
            diag::log(format!("keep connected: {name} не отвечает, переходим на {next_name}"));
            st.store.settings.last_server = Some(next);
            save(&mut st);
            restore_selection(&ui, &st);
            flash_status_for(&ui, &app, &mut st, format!("«{name}» не отвечает, переключаемся на «{next_name}»"), 8);
            drop(st);
            switch_connection(&ui, &app, false);
        });
    });
}

/// Where to move from the server `key`: another of its subscription, see
/// [`failover_pick`]. (key, name).
fn failover_target(st: &App, key: &str) -> Option<(String, String)> {
    let sub = st.store.subscriptions.iter().find(|s| s.servers.iter().any(|x| key_of(s, x) == key))?;
    let servers: Vec<(String, Option<(i32, u128)>)> = sub
        .servers
        .iter()
        .map(|s| key_of(sub, s))
        .map(|k| {
            let ping = st.ping_results.get(&k).map(|(_, level, ms)| (*level, *ms));
            (k, ping)
        })
        .collect();
    let i = failover_pick(&servers, key, &st.failover_tried)?;
    Some((servers[i].0.clone(), display_name(&sub.servers[i].name).1))
}

/// Of `servers` (key, last ping as (level, ms)): the fastest by the last
/// ping, then the ones not pinged in list order. Not `current`, not one
/// already tried, not one whose last ping failed (level 4).
fn failover_pick(servers: &[(String, Option<(i32, u128)>)], current: &str, tried: &std::collections::HashSet<String>) -> Option<usize> {
    servers
        .iter()
        .enumerate()
        .filter(|(_, (k, ping))| k != current && !tried.contains(k) && !matches!(ping, Some((4, _))))
        .min_by_key(|(i, (_, ping))| (ping.map_or(u128::MAX, |(_, ms)| ms), *i))
        .map(|(i, _)| i)
}

/// Reads the traffic counters of the active connection, every second while
/// the window is shown.
fn measure_traffic(ui: &AppWindow, app: &Shared, st: &mut App) {
    let (ConnState::Connected { .. }, Some(api)) = (&st.conn_state, st.conn.as_ref().and_then(|c| c.api)) else { return };
    if st.traffic_busy {
        return;
    }
    st.traffic_busy = true;
    let generation = st.conn_gen;
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let counters = exit::traffic(api);
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.traffic_busy = false;
            if st.conn_gen != generation || !matches!(st.conn_state, ConnState::Connected { .. }) {
                return;
            }
            if let Ok(counters) = counters {
                st.traffic = Some(Traffic::read(st.traffic.as_ref(), std::time::Instant::now(), counters));
                render(&ui, &st);
            }
        });
    });
}

/// Where the connection comes out: the balancer's node (if any), then the
/// exit country through the tunnel. Asked right after connecting and every
/// 30 s while the window is shown; a failed reading keeps the last one.
fn measure_exit(ui: &AppWindow, app: &Shared, st: &mut App) {
    let (ConnState::Connected { .. }, Some(conn)) = (&st.conn_state, &st.conn) else { return };
    if st.exit_busy {
        return;
    }
    st.exit_busy = true;
    let generation = st.conn_gen;
    let (socks, user, pass) = (conn.socks, conn.user.clone(), conn.pass.clone());
    let balancer = conn.balancer.clone();
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        let node = balancer.as_ref().map(|(api, b)| exit::selected_node(*api, &b.tag).ok().flatten());
        // Until the balancer has picked a node, its traffic goes to the
        // fallback (usually "direct"): a trace then would show the user's own
        // country. Ask again shortly instead.
        let trace = match node {
            Some(None) => Err(anyhow::anyhow!("the balancer has not picked a node yet")),
            _ => exit::trace(socks, &user, &pass),
        };
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            let mut st = app.lock().unwrap();
            st.exit_busy = false;
            if st.conn_gen != generation || !matches!(st.conn_state, ConnState::Connected { .. }) {
                return;
            }
            match trace {
                Ok(t) => st.exit = Some(t),
                // Right after connecting: retry every 2 s, not at the 30 s tick.
                Err(_) if st.exit.is_none() => {
                    let (ui_weak, app) = (ui.as_weak(), app.clone());
                    slint::Timer::single_shot(std::time::Duration::from_secs(2), move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        let mut st = app.lock().unwrap();
                        if st.conn_gen == generation {
                            measure_exit(&ui, &app, &mut st);
                        }
                    });
                }
                Err(_) => {}
            }
            // Only a node with a name says something; a bare tag or index does not.
            st.exit_node = node.flatten().and_then(|tag| st.node_names.get(&tag)).map(|name| display_name(name).1);
            render(&ui, &st);
        });
    });
}

/// Speed-tests `jobs` (key, server) one after another, each through a
/// throwaway xray with its config; the results show on the rows. One run at
/// a time: it saturates the link.
fn speed_jobs(ui: &AppWindow, app: &Shared, mut st: std::sync::MutexGuard<App>, jobs: Vec<(String, duoray_core::server::Server)>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    if st.speed_cancel.is_some() {
        flash_status(ui, app, &mut st, "Тест скорости уже идёт".into());
        return render(ui, &st);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    st.speed_cancel = Some(cancel.clone());
    st.speed_progress = Some((0, jobs.len()));
    for (key, _) in &jobs {
        st.speed_results.insert(key.clone(), SpeedCell::Queued);
    }
    let source = match st.store.settings.speedtest {
        SpeedSource::Ookla => speed::Source::Ookla,
        SpeedSource::Cloudflare => speed::Source::Cloudflare,
        SpeedSource::Own => speed::Source::Own(st.store.settings.speedtest_url.clone()),
    };
    let run_dir = st.path.parent().map(|p| p.join("run")).unwrap_or_else(|| PathBuf::from("run"));
    render(ui, &st);
    drop(st);
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    std::thread::spawn(move || {
        // Applies `change` to the state and redraws.
        let update = |change: Box<dyn FnOnce(&mut App) + Send>| {
            let app = app.clone();
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let mut st = app.lock().unwrap();
                change(&mut st);
                render(&ui, &st);
            });
        };
        let iface = connection::uplink_interface();
        for (done, (key, server)) in jobs.iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let (k, name, total) = (key.clone(), display_name(&server.name).1, jobs.len());
            update(Box::new(move |st| {
                st.speed_results.insert(k, SpeedCell::Testing { down: None, up: None });
                st.speed_progress = Some((done, total));
                st.speed_live = Some(SpeedLive { server: name, target: String::new(), phase: "подключение…".into() });
            }));
            let progress = |p: speed::Progress| {
                let k = key.clone();
                update(Box::new(move |st| {
                    let (Some(live), Some(SpeedCell::Testing { down, up })) = (st.speed_live.as_mut(), st.speed_results.get_mut(&k)) else {
                        return;
                    };
                    match p {
                        speed::Progress::Target(t) => {
                            live.target = t;
                            live.phase = "загрузка…".into();
                            (*down, *up) = (None, None);
                        }
                        speed::Progress::Down(r) => {
                            live.phase = format!("загрузка {} Мбит/с", speed::number(r));
                            *down = Some(r);
                        }
                        speed::Progress::Up(r) => {
                            live.phase = if r > 0.0 { format!("отдача {} Мбит/с", speed::number(r)) } else { "отдача…".into() };
                            *up = Some(r);
                        }
                    }
                }));
            };
            let result = iface.as_deref().ok_or_else(|| anyhow::anyhow!("нет подключения к сети")).and_then(|iface| {
                ping::through_server(server, iface, &run_dir, "speed", |socks, user, pass| {
                    speed::measure(socks, user, pass, &source, &cancel, &progress)
                })
            });
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let k = key.clone();
            update(Box::new(move |st| {
                st.speed_results.insert(k, result.map_or(SpeedCell::Failed, SpeedCell::Done));
                st.speed_progress = Some((done + 1, total));
            }));
        }
        let keys: Vec<String> = jobs.into_iter().map(|(k, _)| k).collect();
        update(Box::new(move |st| {
            st.speed_cancel = None;
            st.speed_progress = None;
            st.speed_live = None;
            // Stopped: the ones not reached show nothing.
            for key in keys {
                if st.speed_results.get(&key).is_some_and(|c| matches!(c, SpeedCell::Queued | SpeedCell::Testing { .. })) {
                    st.speed_results.remove(&key);
                }
            }
        }));
    });
}

/// Opens the QR dialog for `text`, or says it does not fit in a QR code.
fn show_qr(ui: &AppWindow, app: &Shared, st: &mut App, title: &str, text: &str) {
    match qr::image(text) {
        Some(image) => {
            ui.set_qr_title(emoji::strip(title).trim().to_string().into());
            ui.set_qr_text(text.into());
            ui.set_qr_image(image);
            ui.set_qr_open(true);
        }
        None => {
            flash_status(ui, app, st, "Ссылка слишком длинная для QR-кода. Скопируйте её".into());
            render(ui, st);
        }
    }
}

/// Shows a status line that disappears after a few seconds.
fn flash_status(ui: &AppWindow, app: &Shared, st: &mut App, msg: String) {
    flash_status_for(ui, app, st, msg, 4);
}

/// A status line that stays `secs` seconds.
fn flash_status_for(ui: &AppWindow, app: &Shared, st: &mut App, msg: String, secs: u64) {
    st.status = msg;
    st.status_gen += 1;
    let generation = st.status_gen;
    let (ui_weak, app) = (ui.as_weak(), app.clone());
    slint::Timer::single_shot(std::time::Duration::from_secs(secs), move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        let mut st = app.lock().unwrap();
        if st.status_gen == generation {
            st.status.clear();
            render(&ui, &st);
        }
    });
}

/// Adds what the user typed or pasted: a subscription URL, or share links.
/// A `duoray://` link: the "Добавить" dialog with its subscription filled in,
/// for the user to confirm (a link from anywhere must not add one silently).
fn open_link(ui: &AppWindow, link: &str) {
    diag::log("duoray:// link opened");
    match deeplink::subscription(link) {
        Some(url) => {
            ui.set_add_error("".into());
            ui.set_add_prefill(url.into());
            ui.set_adding(true);
        }
        None => {
            ui.set_add_error("Ссылка duoray:// не распознана: в ней нет адреса подписки.".into());
            ui.set_adding(true);
        }
    }
}

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

/// The fastest answering server of the subscription `id` by the last ping:
/// selected and connected to (switched to, while connected).
fn connect_fastest(ui: &AppWindow, app: &Shared, id: &str) {
    let mut st = app.lock().unwrap();
    let Some(sub) = st.store.subscriptions.iter().find(|s| s.id == id) else { return };
    let best = sub
        .servers
        .iter()
        .filter_map(|s| {
            let key = key_of(sub, s);
            let (_, level, ms) = st.ping_results.get(&key)?;
            (1..=3).contains(level).then(|| (*ms, key, display_name(&s.name).1))
        })
        .min_by_key(|(ms, ..)| *ms);
    let Some((ms, key, name)) = best else {
        flash_status_for(ui, app, &mut st, "Ни один сервер подписки не ответил".into(), 6);
        return render(ui, &st);
    };
    diag::log(format!("fastest: {ms} ms"));
    flash_status_for(ui, app, &mut st, format!("Самый быстрый: «{name}», {ms} мс"), 6);
    drop(st);
    connect_to(ui, app, key);
}

fn start_ping(ui: &AppWindow, app: &Shared) {
    let st = app.lock().unwrap();
    let Some(sub) = current(ui, &st) else { return };
    let jobs = sub.servers.iter().map(|s| (key_of(sub, s), s.clone())).collect();
    ping_jobs(ui, app, st, jobs);
}

/// Pings `jobs` (key, server) unless a run is already active: runs share the
/// per-worker xray config files.
fn ping_jobs(
    ui: &AppWindow,
    app: &Shared,
    mut st: std::sync::MutexGuard<App>,
    jobs: Vec<(String, duoray_core::server::Server)>,
) {
    if st.ping_progress.is_some() || jobs.is_empty() {
        return;
    }
    for (key, _) in &jobs {
        st.ping_results.insert(key.clone(), ("…".into(), 5, u128::MAX - 1));
    }
    st.ping_progress = Some((0, jobs.len()));
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    st.ping_cancel = Some(cancel.clone());
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
                let entry = ping_entry(result.ok().map(|d| d.as_millis()));
                st.ping_results.insert(key, entry);
                if let Some((done, _)) = st.ping_progress.as_mut() {
                    *done += 1;
                }
                // Hundreds of results: one list rebuild per 150 ms, not per result.
                if !st.ping_render_pending {
                    st.ping_render_pending = true;
                    let (ui_weak, app) = (ui.as_weak(), app.clone());
                    slint::Timer::single_shot(std::time::Duration::from_millis(150), move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        let mut st = app.lock().unwrap();
                        st.ping_render_pending = false;
                        render(&ui, &st);
                    });
                }
            });
        }
    };
    let on_done = {
        let (ui_weak, app) = (ui.as_weak(), app.clone());
        move || {
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                let mut st = app.lock().unwrap();
                st.ping_progress = None;
                st.ping_cancel = None;
                // Cancelled: servers never checked lose their "…".
                st.ping_results.retain(|_, r| r.1 != 5);
                save_pings(&st);
                ui.set_pinging(false);
                render(&ui, &st);
                if let Some(id) = st.connect_best.take() {
                    drop(st);
                    connect_fastest(&ui, &app, &id);
                }
            });
        }
    };
    ping::run_all(jobs, settings, run_dir, cancel, on_result, on_done);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_picks_the_fastest_untried_answering_server() {
        use std::collections::HashSet;
        let s = |k: &str, ping: Option<(i32, u128)>| (k.to_string(), ping);
        let servers = [s("a", Some((1, 40))), s("b", Some((4, u128::MAX))), s("c", None), s("d", Some((2, 200))), s("e", Some((1, 90)))];
        // e is the fastest other one; b failed its last ping.
        assert_eq!(failover_pick(&servers, "a", &HashSet::new()), Some(4));
        // Pinged ones first, then the ones not pinged.
        let tried: HashSet<String> = ["e", "d"].map(String::from).into();
        assert_eq!(failover_pick(&servers, "a", &tried), Some(2));
        let all: HashSet<String> = ["c", "d", "e"].map(String::from).into();
        assert_eq!(failover_pick(&servers, "a", &all), None);
    }

    #[test]
    fn traffic_counts_across_xray_restarts() {
        let t0 = std::time::Instant::now();
        let a = Traffic::read(None, t0, (100, 1000));
        assert_eq!(a.session, (100, 1000));
        let b = Traffic::read(Some(&a), t0 + std::time::Duration::from_secs(2), (300, 5000));
        assert_eq!((b.session, b.rate), ((300, 5000), (100.0, 2000.0)));
        // xray started again: its counters begin from zero.
        let c = Traffic::read(Some(&b), t0 + std::time::Duration::from_secs(3), (50, 400));
        assert_eq!(c.session, (350, 5400));
        assert_eq!(c.label(), "↓ 400 Б/с · ↑ 50 Б/с · 5.6 КБ");
    }

    #[test]
    fn server_filter() {
        let keep = ServerFilter::new("(lte|обход|россия)", false).unwrap();
        assert!(keep.shows("🇷🇺 Россия (Москва 0x)", ""), "case-insensitive, Cyrillic");
        assert!(keep.shows("🇪🇪 LTE #1 (1x)", ""));
        assert!(keep.shows("Германия", "Для обхода ограничений"), "matches the description too");
        assert!(!keep.shows("Германия (Бавария 0x)", "Безлимитный"));

        let hide = ServerFilter::new("(lte|обход|россия)", true).unwrap();
        assert!(!hide.shows("LTE #2 (1x)", ""));
        assert!(hide.shows("Германия (Бавария 0x)", "Безлимитный"));

        assert!(ServerFilter::new("   ", false).unwrap().re.is_none(), "blank: no filter");
        assert!(ServerFilter::new("(lte", false).is_err(), "a broken pattern is an error, not a filter");
    }

    #[test]
    fn expiry_reminder() {
        let now = 1_800_000_000;
        let sub = |id: &str, expire: Option<u64>| Subscription {
            id: id.into(),
            info: subscription::SubInfo { expire, ..Default::default() },
            ..Default::default()
        };
        let mut subs = vec![sub("far", Some(now + 10 * 86400)), sub("unlimited", Some(0)), sub("none", None)];
        assert!(expiring(&subs, now).is_none());
        subs.push(sub("soon", Some(now + 2 * 86400)));
        subs.push(sub("sooner", Some(now + 3600)));
        assert_eq!(expiring(&subs, now).map(|s| s.id.as_str()), Some("sooner"));
        subs[4].expiry_dismissed = Some(now + 3600);
        assert_eq!(expiring(&subs, now).map(|s| s.id.as_str()), Some("soon"), "closed one is skipped");
        subs[4].info.expire = Some(now + 30 * 86400);
        assert_eq!(expiring(&subs, now + 3 * 86400).map(|s| s.id.as_str()), Some("soon"), "expired, within a week");
        assert!(expiring(&subs, now + 21 * 86400).is_none(), "a week after the end it stops");

        assert_eq!(expiry_text("X", now - 1, now), "Подписка «X» закончилась");
        assert!(expiry_text("X", now + 5 * 86400, now).contains("через 5 дней"));
    }

    #[test]
    fn pings_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("duoray-pings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("store.json");
        std::fs::write(pings_path(&store), r#"{"a":120,"b":null,"c":900}"#).unwrap();
        let pings = load_pings(&store);
        assert_eq!(pings["a"], ("120 мс".to_string(), 1, 120));
        assert_eq!(pings["b"].1, 4);
        assert_eq!(pings["c"].1, 3);
        // A broken file is ignored, not fatal.
        std::fs::write(pings_path(&store), "not json").unwrap();
        assert!(load_pings(&store).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn formatting() {
        assert_eq!(plural(1, "сервер", "сервера", "серверов"), "1 сервер");
        assert_eq!(plural(3, "сервер", "сервера", "серверов"), "3 сервера");
        assert_eq!(plural(11, "сервер", "сервера", "серверов"), "11 серверов");
        assert_eq!(plural(56, "сервер", "сервера", "серверов"), "56 серверов");
        assert_eq!(bytes(92_156_218_401), "85.8 ГБ");
        assert_eq!(date(1_798_761_600), "01.01.2027");
    }

    #[test]
    fn shortcuts_on_non_latin_layouts() {
        use slint::winit_030::winit::keyboard::{KeyCode, ModifiersState as M};
        assert_eq!(latin_shortcut("м", KeyCode::KeyV, M::CONTROL), Some('v'));
        assert_eq!(latin_shortcut("я", KeyCode::KeyZ, M::CONTROL | M::SHIFT), Some('Z'));
        assert_eq!(latin_shortcut("с", KeyCode::KeyC, M::SUPER), Some('c'));
        // Latin layouts, plain typing, AltGr and non-letter keys stay as they are.
        assert_eq!(latin_shortcut("v", KeyCode::KeyV, M::CONTROL), None);
        assert_eq!(latin_shortcut("м", KeyCode::KeyV, M::empty()), None);
        assert_eq!(latin_shortcut("м", KeyCode::KeyV, M::CONTROL | M::ALT), None);
        assert_eq!(latin_shortcut("ж", KeyCode::Semicolon, M::CONTROL), None);
    }
}

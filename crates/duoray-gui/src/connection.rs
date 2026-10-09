//! One VPN connection: xray (as the user) + TUN (through the helper).

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use duoray_core::helper_proto::{AppsRequest, PROTOCOL_APPS, TunRequest};
use duoray_core::routing::AppMode;
use duoray_core::geo::GeoDir;
use duoray_core::routing::{GeoData, RoutingSettings};
use duoray_core::runtime;
use duoray_core::server::Server;

use crate::helper::{HelperSession, OpenError};

pub enum ConnectError {
    HelperMissing,
    HelperOutdated(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for ConnectError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

impl From<std::io::Error> for ConnectError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.into())
    }
}

impl From<serde_json::Error> for ConnectError {
    fn from(e: serde_json::Error) -> Self {
        Self::Other(e.into())
    }
}

pub struct Connection {
    helper: Arc<Mutex<HelperSession>>,
    xray: Arc<Mutex<Child>>,
    /// Tells the watcher that the teardown is intentional.
    closing: Arc<std::sync::atomic::AtomicBool>,
    pub tun: String,
    /// xray's SOCKS inbound, used for the live latency check.
    pub socks: SocketAddr,
    pub user: String,
    pub pass: String,
    /// Skipped routing entries and compatibility warnings shown on the routing page.
    pub warnings: Vec<String>,
    /// The config dials Hysteria somewhere (directly or in a balancer).
    pub hysteria: bool,
    respawn: Respawn,
    /// What the tunnel was set up with: a routing change that keeps these can
    /// be applied by restarting xray alone (see [`Connection::rerouter`]).
    tunnel: TunnelInputs,
}

#[derive(Clone)]
struct TunnelInputs {
    server: Server,
    iface: String,
    apps: duoray_core::routing::AppRouting,
    bypass: Vec<std::net::IpAddr>,
    direct_socks: Option<SocketAddr>,
}

/// Everything needed to start the same xray again.
#[derive(Clone)]
struct Respawn {
    config: PathBuf,
    log: PathBuf,
    assets: Option<PathBuf>,
    pid_file: PathBuf,
    socks: SocketAddr,
}

/// `on_failure` is called (from another thread) if the TUN or xray dies,
/// `on_uplink_change` (Linux) when the network moved to another interface:
/// xray is bound to the old one, so the connection has to be made again.
pub fn connect(
    server: &Server,
    run_dir: &Path,
    routing: &RoutingSettings,
    geo: &GeoDir,
    on_failure: impl Fn(String) + Send + Sync + Clone + 'static,
    on_uplink_change: impl Fn() + Send + 'static,
) -> Result<Connection, ConnectError> {
    let mut helper = HelperSession::open().map_err(|e| match e {
        OpenError::Missing => ConnectError::HelperMissing,
        OpenError::Outdated(v) => ConnectError::HelperOutdated(v),
        OpenError::Other(e) => ConnectError::Other(e),
    })?;

    // Per-app routing needs a helper that knows it.
    if routing.apps.active() && helper.protocol < PROTOCOL_APPS {
        return Err(ConnectError::HelperOutdated(helper.version.clone()));
    }
    let iface = physical_interface()?;
    // Fresh databases if downloaded, else the ones shipped with xray (then
    // tags missing there, like geoip:ru-whitelist, are skipped).
    let assets = geo.assets().map(Path::to_path_buf).or_else(|| find_xray().ok().and_then(|(_, a)| a));
    let geo_data = GeoData::load(assets.as_deref(), Some(&geo.lists()));
    let rt = runtime::build(server, &iface, Some(runtime::Routing { settings: routing, geo: &geo_data }))
        .context("preparing xray config")?;
    let warnings = rt.warnings.clone();
    #[cfg(target_os = "macos")]
    let warnings = {
        let mut warnings = warnings;
        if routing.apps.active()
            && Command::new("/bin/ps").args(["-axo", "comm="]).output().ok().is_some_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout).lines().any(|line| {
                        Path::new(line.trim()).file_name().is_some_and(|name| name == "com.adguard.mac.adguard.network-extension")
                    })
            })
        {
            warnings.push(
                "Работает сетевое расширение AdGuard. При фильтрации оно может открывать соединения от своего имени: \
                 правила Chrome и Zen тогда не сработают. Исключите выбранные приложения из фильтрации AdGuard \
                 или временно выключите его защиту для проверки, затем полностью перезапустите браузер.".into(),
            );
        }
        warnings
    };
    let hysteria = rt.config["outbounds"]
        .as_array()
        .is_some_and(|o| o.iter().any(|ob| matches!(ob["protocol"].as_str(), Some("hysteria" | "hysteria2"))));

    std::fs::create_dir_all(run_dir)?;
    kill_stale_xray(run_dir);
    let config_path = run_dir.join("xray.json");
    write_private(&config_path, &serde_json::to_vec_pretty(&rt.config)?)?;
    let log_path = run_dir.join("xray.log");
    let mut child = spawn_xray(&config_path, &log_path, assets.as_deref())?;
    let pid_file = run_dir.join("xray.pid");
    let _ = std::fs::write(&pid_file, child.id().to_string());
    let respawn = Respawn {
        config: config_path.clone(),
        log: log_path.clone(),
        assets: assets.clone(),
        pid_file,
        socks: rt.socks,
    };

    if let Err(e) = wait_listening(rt.socks, &mut child, Duration::from_secs(20)) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ConnectError::Other(e.context(log_tail(&log_path))));
    }

    let (socks, user, pass) = (rt.socks, rt.user.clone(), rt.pass.clone());
    let tunnel = TunnelInputs {
        server: server.clone(),
        iface: iface.clone(),
        apps: routing.apps.clone(),
        bypass: rt.bypass.clone(),
        direct_socks: rt.direct_socks,
    };
    let tun = match helper.start(TunRequest {
        socks: rt.socks,
        user: rt.user,
        pass: rt.pass,
        bypass: rt.bypass,
        ipv6: true,
        apps: rt.direct_socks.map(|direct_socks| AppsRequest {
            only: routing.apps.mode == AppMode::Only,
            apps: routing.apps.apps.clone(),
            direct_socks,
        }),
    }) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ConnectError::Other(e.context("TUN")));
        }
    };

    let xray = Arc::new(Mutex::new(child));
    let helper = Arc::new(Mutex::new(helper));
    let closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Watchdog: the helper never pushes messages, so ask it (and check xray).
    std::thread::spawn({
        let (xray, helper, closing) = (xray.clone(), helper.clone(), closing.clone());
        let log_path = log_path.clone();
        let iface = iface.clone();
        move || {
            let gone = |closing: &std::sync::atomic::AtomicBool| closing.load(std::sync::atomic::Ordering::SeqCst);
            let mut moved = 0;
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if gone(&closing) {
                    return;
                }
                // Twice in a row: interfaces come and go for a moment while switching.
                if cfg!(target_os = "linux") {
                    match uplink_interface() {
                        Some(now) if now != iface => moved += 1,
                        _ => moved = 0,
                    }
                    if moved >= 2 {
                        return on_uplink_change();
                    }
                }
                if let Ok(Some(status)) = xray.lock().unwrap().try_wait() {
                    if !gone(&closing) {
                        on_failure(format!("xray остановился ({status}). {}", log_tail(&log_path)));
                    }
                    return;
                }
                let status = helper.lock().unwrap().status();
                if gone(&closing) {
                    return;
                }
                match status {
                    Ok(None) => {}
                    Ok(Some(reason)) => return on_failure(format!("Туннель остановился: {reason}")),
                    Err(_) => return on_failure(String::new()),
                }
            }
        }
    });

    Ok(Connection { helper, xray, closing, tun, socks, user, pass, warnings, hysteria, respawn, tunnel })
}

impl Connection {
    /// Restarts xray with the same config, port and credentials while the TUN
    /// stays up: open connections drop, new ones go through the fresh xray.
    /// Returns a job to run off the UI thread. The watchdog shares the lock,
    /// so it never sees the old process exit.
    pub fn xray_restarter(&self) -> impl FnOnce() -> Result<()> + Send + 'static {
        let (xray, r) = (self.xray.clone(), self.respawn.clone());
        move || {
            let mut child = xray.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
            let mut fresh = spawn_xray(&r.config, &r.log, r.assets.as_deref())?;
            let _ = std::fs::write(&r.pid_file, fresh.id().to_string());
            let ready = wait_listening(r.socks, &mut fresh, Duration::from_secs(20));
            // Keep it even if not ready yet: the watchdog reports a dead one.
            *child = fresh;
            ready.map_err(|e| e.context(log_tail(&r.log)))
        }
    }

    /// A job that applies new routing without touching the tunnel: a fresh
    /// xray config on the same SOCKS port and credentials, then an xray
    /// restart (open connections drop, the VPN stays up). `None` when the
    /// change needs a new tunnel: other per-app rules. The job returns
    /// `Ok(None)` if it finds that the server's addresses changed (the tunnel
    /// lets the old ones past itself): reconnect then.
    pub fn rerouter(
        &self,
        routing: &RoutingSettings,
        geo: &GeoDir,
    ) -> Option<impl FnOnce() -> Result<Option<Vec<String>>> + Send + 'static> {
        if routing.apps != self.tunnel.apps {
            return None;
        }
        let (routing, t, r, xray) = (routing.clone(), self.tunnel.clone(), self.respawn.clone(), self.xray.clone());
        let (user, pass, lists) = (self.user.clone(), self.pass.clone(), geo.lists());
        Some(move || {
            let geo_data = GeoData::load(r.assets.as_deref(), Some(&lists));
            let mut rt = runtime::build(&t.server, &t.iface, Some(runtime::Routing { settings: &routing, geo: &geo_data }))
                .context("preparing xray config")?;
            if rt.bypass != t.bypass || rt.direct_socks.is_some() != t.direct_socks.is_some() {
                return Ok(None);
            }
            keep_inbounds(&mut rt.config, r.socks.port(), t.direct_socks.map(|a| a.port()), &user, &pass);
            write_private(&r.config, &serde_json::to_vec_pretty(&rt.config)?)?;
            let mut child = xray.lock().unwrap();
            let _ = child.kill();
            let _ = child.wait();
            let mut fresh = spawn_xray(&r.config, &r.log, r.assets.as_deref())?;
            let _ = std::fs::write(&r.pid_file, fresh.id().to_string());
            let ready = wait_listening(r.socks, &mut fresh, Duration::from_secs(20));
            *child = fresh;
            ready.map_err(|e| e.context(log_tail(&r.log)))?;
            Ok(Some(rt.warnings))
        })
    }

    /// TUN down first (network restored), then xray.
    pub fn disconnect(self) {
        self.closing.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.helper.lock().unwrap().stop();
        drop(self.helper);
        let mut x = self.xray.lock().unwrap();
        let _ = x.kill();
        let _ = x.wait();
    }
}

/// Points a fresh config's SOCKS inbounds at the port and credentials the
/// tunnel already uses.
fn keep_inbounds(config: &mut serde_json::Value, socks: u16, direct: Option<u16>, user: &str, pass: &str) {
    for inbound in config["inbounds"].as_array_mut().into_iter().flatten() {
        let port = match inbound["tag"].as_str() {
            Some(duoray_core::routing::DIRECT_INBOUND) => direct,
            _ => Some(socks),
        };
        if let Some(port) = port {
            inbound["port"] = serde_json::json!(port);
            inbound["settings"]["accounts"] = serde_json::json!([{ "user": user, "pass": pass }]);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rerouted_config_keeps_the_tunnel_endpoints() {
        let mut config = serde_json::json!({ "inbounds": [
            { "tag": "socks", "port": 1111, "settings": { "auth": "password", "accounts": [{ "user": "new", "pass": "new" }] } },
            { "tag": duoray_core::routing::DIRECT_INBOUND, "port": 2222, "settings": { "accounts": [{ "user": "new", "pass": "new" }] } },
        ]});
        super::keep_inbounds(&mut config, 40000, Some(40001), "u", "p");
        assert_eq!(config["inbounds"][0]["port"], 40000);
        assert_eq!(config["inbounds"][1]["port"], 40001);
        for i in 0..2 {
            assert_eq!(config["inbounds"][i]["settings"]["accounts"], serde_json::json!([{ "user": "u", "pass": "p" }]));
        }
        assert_eq!(config["inbounds"][0]["settings"]["auth"], "password");
    }
}

pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
        Ok(())
    }
}

/// The uplink xray must bind to. Skips tunnels: another VPN may own the top route.
/// The real uplink even when another VPN owns the top route; used by ping so
/// probes never go through someone else's tunnel.
/// No console window for helper processes on Windows (the GUI has none).
pub fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Default-route adapters by preference: (alias, looks like a VPN).
#[cfg(windows)]
fn windows_default_aliases() -> Result<Vec<(String, bool)>> {
    // The IP Helper API directly: a PowerShell start costs about two seconds.
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfEntry2, GetIpForwardTable2, GetIpInterfaceEntry, MIB_IF_ROW2, MIB_IPFORWARD_TABLE2,
        MIB_IPINTERFACE_ROW,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;
    let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    // SAFETY: on success the table is ours until FreeMibTable.
    let err = unsafe { GetIpForwardTable2(AF_INET, &mut table) };
    if err != 0 {
        bail!("GetIpForwardTable2: error {err}");
    }
    // SAFETY: the table holds NumEntries rows.
    let rows = unsafe { std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize) };
    let wide = |s: &[u16]| String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]);
    let mut found: Vec<(u32, String, bool)> = vec![];
    for row in rows.iter().filter(|r| r.DestinationPrefix.PrefixLength == 0) {
        let mut iface = MIB_IPINTERFACE_ROW { Family: AF_INET, InterfaceLuid: row.InterfaceLuid, ..Default::default() };
        // SAFETY: Family and InterfaceLuid identify the row to fill.
        if unsafe { GetIpInterfaceEntry(&mut iface) } != 0 || !iface.Connected {
            continue;
        }
        let mut entry = MIB_IF_ROW2 { InterfaceLuid: row.InterfaceLuid, ..Default::default() };
        // SAFETY: InterfaceLuid identifies the row to fill.
        if unsafe { GetIfEntry2(&mut entry) } != 0 {
            continue;
        }
        let (alias, desc) = (wide(&entry.Alias), wide(&entry.Description));
        let hay = format!("{alias} {desc}").to_lowercase();
        let vpn = ["duoray", "wintun", "wireguard", "tap-", "tailscale", "openvpn", "happ", "throne", "hiddify", "nekoray", "sing-tun", "clash", "mihomo", "v2ray", "xray"]
            .iter()
            .any(|k| hay.contains(k));
        found.push((row.Metric.saturating_add(iface.Metric), alias, vpn));
    }
    // SAFETY: allocated by GetIpForwardTable2 above.
    unsafe { FreeMibTable(table.cast()) };
    found.sort_by_key(|f| f.0);
    Ok(found.into_iter().map(|(_, a, vpn)| (a, vpn)).collect())
}

#[cfg(windows)]
fn windows_uplink() -> Result<String> {
    let aliases = windows_default_aliases()?;
    match aliases.first() {
        None => bail!("не найден маршрут по умолчанию"),
        Some((a, true)) => bail!("активен другой VPN ({a}). Выключите его и попробуйте снова"),
        Some((a, false)) => Ok(a.clone()),
    }
}

pub fn uplink_interface() -> Option<String> {
    #[cfg(windows)]
    return windows_default_aliases().ok()?.into_iter().find(|(_, vpn)| !vpn).map(|(a, _)| a);
    #[cfg(target_os = "linux")]
    return linux_default_devs().ok()?.into_iter().find(|d| !is_tunnel_dev(d));
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/usr/sbin/netstat").args(["-rn", "-f", "inet"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        return text
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .filter(|c| c.len() >= 4 && c[0] == "default")
            .map(|c| c[3])
            .find(|i| !["utun", "bridge", "ipsec", "ppp", "gif", "stf"].iter().any(|p| i.starts_with(p)))
            .map(String::from);
    }
    #[allow(unreachable_code)]
    None
}

/// Default-route devices from the main table, best (lowest metric) first.
#[cfg(target_os = "linux")]
fn linux_default_devs() -> Result<Vec<String>> {
    let out = Command::new("ip").args(["-j", "-4", "route", "show", "default"]).output().context("ip route")?;
    let routes: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).context("ip -j route")?;
    let mut routes: Vec<(u64, String)> = routes
        .iter()
        .filter_map(|r| Some((r["metric"].as_u64().unwrap_or(0), r["dev"].as_str()?.to_string())))
        .collect();
    routes.sort();
    Ok(routes.into_iter().map(|(_, d)| d).collect())
}

#[cfg(target_os = "linux")]
fn is_tunnel_dev(dev: &str) -> bool {
    ["duoray", "tun", "tap", "wg", "utun", "tailscale", "ppp", "zt", "nekoray", "throne", "sing"]
        .iter()
        .any(|p| dev.starts_with(p))
}

pub fn physical_interface() -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        let devs = linux_default_devs()?;
        return match devs.first() {
            None => bail!("нет подключения к сети (не найден маршрут по умолчанию)"),
            Some(d) if is_tunnel_dev(d) => bail!("активен другой VPN ({d}). Выключите его и попробуйте снова"),
            Some(d) => Ok(d.clone()),
        };
    }
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("/usr/sbin/netstat").args(["-rn", "-f", "inet"]).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut first = None;
        for cols in text.lines().map(|l| l.split_whitespace().collect::<Vec<_>>()) {
            if cols.len() >= 4 && cols[0] == "default" {
                let iface = cols[3];
                if iface.starts_with("utun") {
                    first.get_or_insert("tunnel");
                    continue;
                }
                if !iface.starts_with("bridge") && !iface.starts_with("ipsec") && !iface.starts_with("ppp") {
                    if first == Some("tunnel") {
                        bail!("активен другой VPN (Happ, Throne, …). Выключите его и попробуйте снова");
                    }
                    return Ok(iface.to_string());
                }
            }
        }
        bail!("нет подключения к сети (не найден маршрут по умолчанию)")
    }
    #[cfg(windows)]
    return windows_uplink().context("нет подключения к сети");
    #[allow(unreachable_code)]
    {
        bail!("подключение на этой ОС пока не поддерживается")
    }
}

pub fn find_xray() -> Result<(PathBuf, Option<PathBuf>)> {
    let exe_dir = std::env::current_exe()?.parent().map(Path::to_path_buf);
    let name = if cfg!(windows) { "xray.exe" } else { "xray" };
    let mut candidates: Vec<PathBuf> = vec![];
    if let Some(d) = &exe_dir {
        candidates.push(d.join(name));
    }
    // Linux packages put xray (with network capabilities) here.
    candidates.push(PathBuf::from("/usr/lib/duoray/xray"));
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join(name)));
    }
    candidates.extend(["/opt/homebrew/bin/xray", "/usr/local/bin/xray"].map(PathBuf::from));
    // Started from a desktop launcher, PATH may lack the user's own bin dir
    // (where the official install script and manual installs put xray).
    #[cfg(target_os = "linux")]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/bin/xray"));
    }
    let bin = candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("xray не найден: положите его рядом с DUORAY или установите (brew install xray)")?;

    // geoip.dat/geosite.dat: next to xray, next to us, or the Homebrew share dir.
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut dirs = vec![
        bin.parent().map(Path::to_path_buf),
        exe_dir,
        Some(PathBuf::from("/opt/homebrew/share/xray")),
        Some(PathBuf::from("/usr/local/share/xray")),
    ];
    // Distribution packages and the official install script (system or --user).
    #[cfg(target_os = "linux")]
    {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
        dirs.push(data_home.map(|d| d.join("xray")));
        dirs.extend(["/usr/share/xray", "/usr/share/v2ray", "/usr/local/share/v2ray"].map(|d| Some(PathBuf::from(d))));
    }
    let assets = dirs.into_iter().flatten().find(|d| d.join("geoip.dat").is_file());
    Ok((bin, assets))
}

fn spawn_xray(config: &Path, log: &Path, assets: Option<&Path>) -> Result<Child> {
    let (bin, _) = find_xray()?;
    let log_file = std::fs::File::create(log)?;
    let mut cmd = Command::new(bin);
    hidden(&mut cmd);
    cmd.args(["run", "-c"])
        .arg(config)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    if let Some(a) = assets {
        cmd.env("XRAY_LOCATION_ASSET", a);
    }
    #[cfg(target_os = "linux")]
    return linux_spawn::spawn_tied(cmd).context("запуск xray");
    #[allow(unreachable_code)]
    cmd.spawn().context("запуск xray")
}

/// xray must not outlive the GUI, even when the GUI is killed outright:
/// the kernel signals it when its parent goes away (PR_SET_PDEATHSIG).
#[cfg(target_os = "linux")]
mod linux_spawn {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Mutex, OnceLock};

    type Job = (Command, Sender<std::io::Result<Child>>);

    /// The death signal follows the parent *thread*, and connections are made
    /// from short-lived threads, so children are forked from this one, which
    /// lives as long as the process.
    fn spawner() -> &'static Mutex<Sender<Job>> {
        static SPAWNER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
        SPAWNER.get_or_init(|| {
            let (tx, rx) = channel::<Job>();
            std::thread::Builder::new()
                .name("child-spawner".into())
                .spawn(move || {
                    for (mut cmd, reply) in rx {
                        let _ = reply.send(cmd.spawn());
                    }
                })
                .expect("spawner thread");
            Mutex::new(tx)
        })
    }

    pub fn spawn_tied(mut cmd: Command) -> std::io::Result<Child> {
        let parent = std::process::id();
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                // The GUI died before prctl took effect.
                if libc::getppid() as u32 != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let (tx, rx) = channel();
        spawner().lock().unwrap().send((cmd, tx)).map_err(|_| std::io::Error::other("spawner gone"))?;
        rx.recv().map_err(|_| std::io::Error::other("spawner gone"))?
    }
}

pub fn wait_listening(addr: SocketAddr, child: &mut Child, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            bail!("xray завершился при старте ({status})");
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Tells apart "xray never listened" from "something blocks loopback
    // connections" (antivirus, firewall filters) in reports.
    let bound = std::net::TcpListener::bind(addr).is_err();
    bail!(
        "xray не открыл порт за {}с ({})",
        timeout.as_secs(),
        if bound { "порт занят, но подключение к нему не проходит" } else { "порт свободен" }
    )
}

/// An xray left over from a crashed session would keep the port and the old config.
fn kill_stale_xray(run_dir: &Path) {
    let pidfile = run_dir.join("xray.pid");
    let Some(pid) = std::fs::read_to_string(&pidfile).ok().and_then(|p| p.trim().parse::<u32>().ok()) else {
        return;
    };
    #[cfg(unix)]
    {
        let comm = Command::new("/bin/ps").args(["-o", "comm=", "-p", &pid.to_string()]).output();
        if let Ok(out) = comm
            && String::from_utf8_lossy(&out.stdout).trim().ends_with("xray")
        {
            let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
        }
    }
    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let list = hidden(Command::new("tasklist").args(["/FI", &filter, "/FO", "CSV", "/NH"])).output();
        if let Ok(out) = list
            && String::from_utf8_lossy(&out.stdout).to_lowercase().contains("xray.exe")
        {
            let _ = hidden(Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"])).status();
        }
    }
    let _ = std::fs::remove_file(pidfile);
}

pub fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    if tail.is_empty() { String::new() } else { format!("Лог xray: {tail}") }
}

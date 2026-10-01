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
    /// Routing entries that could not be applied (shown on the routing page).
    pub warnings: Vec<String>,
}

/// `on_failure` is called (from another thread) if the TUN or xray dies.
pub fn connect(
    server: &Server,
    run_dir: &Path,
    routing: &RoutingSettings,
    geo: &GeoDir,
    on_failure: impl Fn(String) + Send + Sync + Clone + 'static,
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

    std::fs::create_dir_all(run_dir)?;
    kill_stale_xray(run_dir);
    let config_path = run_dir.join("xray.json");
    write_private(&config_path, &serde_json::to_vec_pretty(&rt.config)?)?;
    let log_path = run_dir.join("xray.log");
    let mut child = spawn_xray(&config_path, &log_path, assets.as_deref())?;
    let _ = std::fs::write(run_dir.join("xray.pid"), child.id().to_string());

    if let Err(e) = wait_listening(rt.socks, &mut child, Duration::from_secs(10)) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ConnectError::Other(e.context(log_tail(&log_path))));
    }

    let (socks, user, pass) = (rt.socks, rt.user.clone(), rt.pass.clone());
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
        move || {
            let gone = |closing: &std::sync::atomic::AtomicBool| closing.load(std::sync::atomic::Ordering::SeqCst);
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if gone(&closing) {
                    return;
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

    Ok(Connection { helper, xray, closing, tun, socks, user, pass, warnings })
}

impl Connection {
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
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
        Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction Stop | \
        Sort-Object RouteMetric | ForEach-Object { $_.InterfaceAlias + '|' + (Get-NetAdapter -InterfaceIndex $_.ifIndex -ErrorAction SilentlyContinue).InterfaceDescription }";
    let out = hidden(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]))
        .output()
        .context("powershell")?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|l| {
            let (alias, desc) = l.trim().split_once('|')?;
            let hay = format!("{alias} {desc}").to_lowercase();
            let vpn = ["duoray", "wintun", "wireguard", "tap-", "tailscale", "openvpn", "happ", "throne", "hiddify", "nekoray", "sing-tun", "clash", "mihomo", "v2ray", "xray"]
                .iter()
                .any(|k| hay.contains(k));
            Some((alias.to_string(), vpn))
        })
        .collect())
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
    ["tun", "tap", "wg", "utun", "tailscale", "ppp", "zt", "nekoray", "throne", "sing"]
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
    let bin = candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("xray не найден: положите его рядом с DUORAY или установите (brew install xray)")?;

    // geoip.dat/geosite.dat: next to xray, next to us, or the Homebrew share dir.
    let assets = [
        bin.parent().map(Path::to_path_buf),
        exe_dir,
        Some(PathBuf::from("/opt/homebrew/share/xray")),
        Some(PathBuf::from("/usr/local/share/xray")),
    ]
    .into_iter()
    .flatten()
    .find(|d| d.join("geoip.dat").is_file());
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
    cmd.spawn().context("запуск xray")
}

pub fn wait_listening(addr: SocketAddr, child: &mut Child, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            bail!("xray завершился при старте ({status})");
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("xray не открыл порт за {}с", timeout.as_secs())
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

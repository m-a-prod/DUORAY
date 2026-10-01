//! Server latency checks: TCP connect, ICMP (system `ping`), or an HTTP GET
//! through the server itself. All of them leave via the physical interface so
//! an active DUORAY tunnel does not skew the numbers.

use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use duoray_core::runtime;
use duoray_core::server::Server;
use duoray_core::store::{PingMode, PingSettings};

use crate::connection;

/// Pings `jobs` with `settings.threads` workers; `on_result` is called from
/// worker threads as results arrive, `on_done` once at the end.
pub fn run_all(
    jobs: Vec<(String, Server)>,
    settings: PingSettings,
    run_dir: PathBuf,
    on_result: impl Fn(String, Result<Duration, String>) + Send + Sync + 'static,
    on_done: impl FnOnce() + Send + 'static,
) {
    let iface = connection::uplink_interface();
    let threads = settings.threads.clamp(1, 32) as usize;
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let on_result = Arc::new(on_result);
    let settings = Arc::new(settings);

    std::thread::spawn(move || {
        let workers: Vec<_> = (0..threads)
            .map(|n| {
                let (queue, on_result, settings, iface, run_dir) =
                    (queue.clone(), on_result.clone(), settings.clone(), iface.clone(), run_dir.clone());
                std::thread::spawn(move || {
                    loop {
                        let Some((key, server)) = queue.lock().unwrap().pop_front() else { return };
                        let result = ping(&server, &settings, iface.as_deref(), &run_dir, n).map_err(|e| format!("{e:#}"));
                        on_result(key, result);
                    }
                })
            })
            .collect();
        for w in workers {
            let _ = w.join();
        }
        on_done();
    });
}

pub fn ping(server: &Server, s: &PingSettings, iface: Option<&str>, run_dir: &Path, worker: usize) -> Result<Duration> {
    let timeout = Duration::from_millis(s.timeout_ms.clamp(500, 30_000));
    match s.mode {
        PingMode::Tcp => tcp(&server.address, server.port, iface, timeout),
        PingMode::Icmp => icmp(&server.address, iface, timeout),
        PingMode::HttpGet => http(server, &s.url, false, iface, timeout, run_dir, worker),
        PingMode::HttpHead => http(server, &s.url, true, iface, timeout, run_dir, worker),
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()
        .with_context(|| format!("не удалось найти {host}"))?
        .find(SocketAddr::is_ipv4)
        .with_context(|| format!("у {host} нет IPv4-адреса"))
}

fn tcp(host: &str, port: u16, iface: Option<&str>, timeout: Duration) -> Result<Duration> {
    use socket2::{Domain, Protocol, Socket, Type};

    let addr = resolve(host, port)?;
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    bind_to(&socket, iface);
    let start = Instant::now();
    socket.connect_timeout(&addr.into(), timeout).map_err(|e| anyhow::anyhow!("нет соединения: {e}"))?;
    Ok(start.elapsed())
}

/// Keeps the probe off the TUN (macOS: IP_BOUND_IF). Elsewhere binding needs
/// privileges, so the probe uses the normal route.
fn bind_to(socket: &socket2::Socket, iface: Option<&str>) {
    #[cfg(target_os = "macos")]
    if let Some(name) = iface
        && let Ok(cname) = std::ffi::CString::new(name)
    {
        // SAFETY: plain libc call with a valid NUL-terminated string.
        let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
        let _ = socket.bind_device_by_index_v4(std::num::NonZeroU32::new(index));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (socket, iface);
}

/// Windows: IcmpSendEcho needs no admin rights and no console output parsing
/// (`ping` prints localized text in the OEM code page).
#[cfg(windows)]
fn icmp(host: &str, _iface: Option<&str>, timeout: Duration) -> Result<Duration> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        ICMP_ECHO_REPLY, IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
    };

    let SocketAddr::V4(addr) = resolve(host, 0)? else { bail!("нужен IPv4-адрес") };
    let dest = u32::from_ne_bytes(addr.ip().octets());
    let payload = [0x44u8; 32];
    let mut reply = vec![0u8; std::mem::size_of::<ICMP_ECHO_REPLY>() + payload.len() + 8];
    // SAFETY: plain Win32 calls; the buffers outlive them and are sized as required.
    unsafe {
        let handle = IcmpCreateFile();
        if handle == INVALID_HANDLE_VALUE {
            bail!("ICMP недоступен");
        }
        let n = IcmpSendEcho(
            handle,
            dest,
            payload.as_ptr().cast(),
            payload.len() as u16,
            std::ptr::null(),
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            timeout.as_millis() as u32,
        );
        IcmpCloseHandle(handle);
        if n == 0 {
            bail!("нет ответа");
        }
        let r = &*(reply.as_ptr() as *const ICMP_ECHO_REPLY);
        if r.Status != 0 {
            bail!("нет ответа");
        }
        Ok(Duration::from_millis(u64::from(r.RoundTripTime.max(1))))
    }
}

#[cfg(not(windows))]
fn icmp(host: &str, iface: Option<&str>, timeout: Duration) -> Result<Duration> {
    let ms = timeout.as_millis().to_string();
    let mut cmd = Command::new("/sbin/ping");
    #[cfg(target_os = "macos")]
    {
        cmd.args(["-c", "1", "-W", &ms]);
        if let Some(i) = iface {
            cmd.args(["-b", i]);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let secs = timeout.as_secs().max(1).to_string();
        cmd = Command::new("ping");
        cmd.args(["-c", "1", "-W", &secs]);
        if let Some(i) = iface {
            cmd.args(["-I", i]);
        }
        let _ = &ms;
    }
    let out = cmd.arg(host).stdin(Stdio::null()).output().context("не удалось запустить ping")?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_ping_time(&text).context("нет ответа")
}

/// Finds "time=12.3 ms" / "time<1ms" / "время=12мс" in ping output.
#[cfg_attr(windows, allow(dead_code))]
fn parse_ping_time(text: &str) -> Option<Duration> {
    for key in ["time=", "time<", "время=", "время<"] {
        if let Some(pos) = text.find(key) {
            let rest = &text[pos + key.len()..];
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            let ms: f64 = num.parse().ok()?;
            return Some(Duration::from_micros((ms * 1000.0) as u64));
        }
    }
    None
}

/// Real delay: a throwaway xray with the server's own config, then one HTTP
/// GET through it. Measures the whole request, handshake included.
fn http(
    server: &Server,
    url: &str,
    head: bool,
    iface: Option<&str>,
    timeout: Duration,
    run_dir: &Path,
    worker: usize,
) -> Result<Duration> {
    let Some(iface) = iface else { bail!("нет подключения к сети") };
    let rt = runtime::build(server, iface)?;
    std::fs::create_dir_all(run_dir)?;
    let config = run_dir.join(format!("ping-{worker}.json"));
    connection::write_private(&config, &serde_json::to_vec(&rt.config)?)?;

    let (bin, assets) = connection::find_xray()?;
    let mut cmd = Command::new(bin);
    cmd.args(["run", "-c"]).arg(&config).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(a) = assets {
        cmd.env("XRAY_LOCATION_ASSET", a);
    }
    let mut child = connection::hidden(&mut cmd).spawn().context("запуск xray")?;
    let result = (|| {
        connection::wait_listening(rt.socks, &mut child, Duration::from_secs(5))?;
        via_socks(rt.socks, &rt.user, &rt.pass, url, head, timeout)
    })();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&config);
    result
}

/// One HTTP request through a SOCKS5 proxy with a fresh connection, so the
/// time includes the handshake with the server (the "real delay").
pub fn via_socks(socks: SocketAddr, user: &str, pass: &str, url: &str, head: bool, timeout: Duration) -> Result<Duration> {
    let proxy = ureq::Proxy::new(&format!("socks5://{user}:{pass}@{socks}"))?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(Some(proxy))
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_idle_connections(0)
        .build()
        .into();
    let start = Instant::now();
    let resp = if head { agent.head(url).call() } else { agent.get(url).call() }
        .map_err(|e| anyhow::anyhow!("нет ответа: {e}"))?;
    let elapsed = start.elapsed();
    if resp.status().is_server_error() {
        bail!("HTTP {}", resp.status());
    }
    Ok(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ping_output() {
        let mac = "64 bytes from 1.1.1.1: icmp_seq=0 ttl=57 time=12.345 ms";
        assert_eq!(parse_ping_time(mac), Some(Duration::from_micros(12345)));
        assert_eq!(parse_ping_time("Reply from 1.1.1.1: bytes=32 time<1ms TTL=57"), Some(Duration::from_millis(1)));
        assert_eq!(parse_ping_time("Ответ от 1.1.1.1: число байт=32 время=24мс TTL=57"), Some(Duration::from_millis(24)));
        assert_eq!(parse_ping_time("Request timeout for icmp_seq 0"), None);
    }
}

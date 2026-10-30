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
use duoray_core::store::{PingMode, PingSettings, ProxyPingMode};

use crate::connection;

/// Pings `jobs` with `settings.threads` workers; `on_result` is called from
/// worker threads as results arrive, `on_done` once at the end. Setting
/// `cancel` stops taking new jobs (running checks still finish).
pub fn run_all(
    jobs: Vec<(String, Server)>,
    settings: PingSettings,
    run_dir: PathBuf,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    on_result: impl Fn(String, Result<Duration, String>) + Send + Sync + 'static,
    on_done: impl FnOnce() + Send + 'static,
) {
    let threads = settings.threads.clamp(1, 32) as usize;
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let on_result = Arc::new(on_result);
    let settings = Arc::new(settings);

    std::thread::spawn(move || {
        // Off the UI thread: on Windows this runs PowerShell (seconds).
        let iface = connection::uplink_interface();
        let workers: Vec<_> = (0..threads)
            .map(|n| {
                let (queue, on_result, settings, iface, run_dir, cancel) =
                    (queue.clone(), on_result.clone(), settings.clone(), iface.clone(), run_dir.clone(), cancel.clone());
                std::thread::spawn(move || {
                    loop {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
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
        PingMode::HttpGet | PingMode::HttpHead => http(server, s, iface, timeout, run_dir, worker),
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

/// Keeps the probe off the TUN (macOS: IP_BOUND_IF, Linux: SO_BINDTODEVICE,
/// unprivileged since Linux 5.7). On Windows binding needs privileges, so the
/// probe uses the normal route.
fn bind_to(socket: &socket2::Socket, iface: Option<&str>) {
    #[cfg(target_os = "linux")]
    if let Some(name) = iface {
        let _ = socket.bind_device(Some(name.as_bytes()));
    }
    #[cfg(target_os = "macos")]
    if let Some(name) = iface
        && let Ok(cname) = std::ffi::CString::new(name)
    {
        // SAFETY: plain libc call with a valid NUL-terminated string.
        let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
        let _ = socket.bind_device_by_index_v4(std::num::NonZeroU32::new(index));
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut cmd = Command::new("/sbin/ping");
        cmd.args(["-c", "1", "-W", &timeout.as_millis().to_string()]);
        if let Some(i) = iface {
            cmd.args(["-b", i]);
        }
        cmd
    };
    #[cfg(not(target_os = "macos"))]
    let cmd = {
        // Location differs between distributions (/bin, /usr/bin): use PATH.
        let mut cmd = Command::new("ping");
        cmd.args(["-c", "1", "-W", &timeout.as_secs().max(1).to_string()]);
        if let Some(i) = iface {
            cmd.args(["-I", i]);
        }
        cmd
    };
    let mut cmd = cmd;
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

/// Real delay: a throwaway xray with the server's own config, then HTTP
/// requests through it according to the selected proxy ping mode.
fn http(server: &Server, settings: &PingSettings, iface: Option<&str>, timeout: Duration, run_dir: &Path, worker: usize) -> Result<Duration> {
    let Some(iface) = iface else { bail!("нет подключения к сети") };
    let rt = runtime::build(server, iface, None)?;
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
        connection::wait_listening(rt.socks, &mut child, Duration::from_secs(5), None)?;
        via_socks_mode(
            rt.socks,
            &rt.user,
            &rt.pass,
            &settings.url,
            settings.mode == PingMode::HttpHead,
            timeout,
            settings.proxy_mode,
        )
    })();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&config);
    result
}

/// HTTP requests through a SOCKS5 proxy. Default: one request on a fresh
/// connection, so the time includes the handshake (the "real delay").
/// Double: two fresh connections in one xray session. KeepAlive: the second
/// request reuses the first one's connection. Both keep the best time.
pub fn via_socks_mode(
    socks: SocketAddr,
    user: &str,
    pass: &str,
    url: &str,
    head: bool,
    timeout: Duration,
    mode: ProxyPingMode,
) -> Result<Duration> {
    // Proxy mode's inbound has no password.
    let proxy = ureq::Proxy::new(&if user.is_empty() { format!("socks5://{socks}") } else { format!("socks5://{user}:{pass}@{socks}") })?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(Some(proxy))
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_idle_connections(if mode == ProxyPingMode::KeepAlive { 1 } else { 0 })
        .build()
        .into();
    let attempts = if mode == ProxyPingMode::Default { 1 } else { 2 };
    let mut best: Option<Duration> = None;
    for attempt in 1..=attempts {
        // KeepAlive reuses the connection only if the body was read to the end.
        let drain = mode == ProxyPingMode::KeepAlive && attempt < attempts;
        // A server that failed once is not asked again: a dead one would cost
        // the timeout twice.
        let elapsed = proxy_request(&agent, url, head, drain)?;
        best = Some(best.map_or(elapsed, |previous| previous.min(elapsed)));
    }
    best.ok_or_else(|| anyhow::anyhow!("нет ответа"))
}

fn proxy_request(agent: &ureq::Agent, url: &str, head: bool, drain: bool) -> Result<Duration> {
    let start = Instant::now();
    let mut resp = if head { agent.head(url).call() } else { agent.get(url).call() }
        .map_err(|e| anyhow::anyhow!("нет ответа: {e}"))?;
    let elapsed = start.elapsed();
    if resp.status().is_server_error() {
        bail!("HTTP {}", resp.status());
    }
    if drain {
        let _ = resp.body_mut().with_config().limit(1024 * 1024).read_to_vec();
    }
    Ok(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn parses_ping_output() {
        let mac = "64 bytes from 1.1.1.1: icmp_seq=0 ttl=57 time=12.345 ms";
        assert_eq!(parse_ping_time(mac), Some(Duration::from_micros(12345)));
        assert_eq!(parse_ping_time("Reply from 1.1.1.1: bytes=32 time<1ms TTL=57"), Some(Duration::from_millis(1)));
        assert_eq!(parse_ping_time("Ответ от 1.1.1.1: число байт=32 время=24мс TTL=57"), Some(Duration::from_millis(24)));
        assert_eq!(parse_ping_time("Request timeout for icmp_seq 0"), None);
    }

    #[test]
    fn proxy_modes_control_connection_reuse() {
        for (mode, connections, requests) in [
            (ProxyPingMode::Default, 1, 1),
            (ProxyPingMode::Double, 2, 1),
            (ProxyPingMode::KeepAlive, 1, 2),
        ] {
            let (address, server) = socks_server(connections, requests);
            via_socks_mode(address, "", "", "http://127.0.0.1/generate_204", false, Duration::from_secs(2), mode).unwrap();
            assert_eq!(server.join().unwrap(), (connections, connections * requests));
        }
    }

    #[test]
    fn failed_first_attempt_is_not_repeated() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        for mode in [ProxyPingMode::Double, ProxyPingMode::KeepAlive] {
            // Accepts and hangs up at once: the SOCKS handshake fails.
            let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = accepted.clone();
            let probe = listener.try_clone().unwrap();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done = stop.clone();
            let server = std::thread::spawn(move || {
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    if let Ok((stream, _)) = probe.accept() {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        drop(stream);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            assert!(via_socks_mode(address, "", "", "http://127.0.0.1/", false, Duration::from_secs(2), mode).is_err());
            std::thread::sleep(Duration::from_millis(200));
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            server.join().unwrap();
            assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1, "{mode:?} asked a failing server twice");
        }
    }

    fn socks_server(connections: usize, requests: usize) -> (SocketAddr, std::thread::JoinHandle<(usize, usize)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            let mut total = 0;
            for _ in 0..connections {
                let deadline = Instant::now() + Duration::from_secs(3);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(accepted) => break accepted,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "SOCKS connection timed out");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("SOCKS accept failed: {e}"),
                    }
                };
                stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut greeting = [0; 2];
                stream.read_exact(&mut greeting).unwrap();
                assert_eq!(greeting[0], 5);
                let mut methods = vec![0; greeting[1] as usize];
                stream.read_exact(&mut methods).unwrap();
                stream.write_all(&[5, 0]).unwrap();

                let mut request = [0; 4];
                stream.read_exact(&mut request).unwrap();
                assert_eq!(&request[..3], &[5, 1, 0]);
                let address_len = match request[3] {
                    1 => 4,
                    3 => {
                        let mut len = [0];
                        stream.read_exact(&mut len).unwrap();
                        len[0] as usize
                    }
                    4 => 16,
                    other => panic!("unsupported SOCKS address type {other}"),
                };
                let mut target = vec![0; address_len + 2];
                stream.read_exact(&mut target).unwrap();
                stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();

                for _ in 0..requests {
                    let mut http = Vec::new();
                    let mut buffer = [0; 1024];
                    while !http.ends_with(b"\r\n\r\n") {
                        let n = stream.read(&mut buffer).unwrap();
                        assert_ne!(n, 0);
                        http.extend_from_slice(&buffer[..n]);
                        assert!(http.len() < 16 * 1024);
                    }
                    total += 1;
                    stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").unwrap();
                    stream.flush().unwrap();
                }
            }
            (connections, total)
        });
        (address, handle)
    }
}

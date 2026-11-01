//! Speed test of a server: download and upload through it, in megabits a
//! second, against the nearest Speedtest (Ookla) server, Cloudflare, or an
//! own file.

use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Cloudflare's speed test endpoints.
const CLOUDFLARE_DOWN: &str = "https://speed.cloudflare.com/__down?bytes=25000000";
const CLOUDFLARE_UP: &str = "https://speed.cloudflare.com/__up";
/// Speedtest servers nearest to who asks: through the server, to its exit.
const OOKLA_SERVERS: &str = "https://www.speedtest.net/api/js/servers?engine=js&limit=5&https_functional=true";
/// Parallel connections to a Speedtest server, as its own client opens.
const OOKLA_STREAMS: usize = 4;
/// Bytes per request to a Speedtest server, each way.
const OOKLA_CHUNK: usize = 25_000_000;
/// Each direction stops after this long, or once the file is in.
const MAX_TIME: Duration = Duration::from_secs(10);
const UPLOAD_BYTES: usize = 10_000_000;

pub enum Source {
    Ookla,
    Cloudflare,
    /// A file to download (no upload).
    Own(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speed {
    pub down_mbps: f64,
    /// Not measured against an own file.
    pub up_mbps: Option<f64>,
}

impl Speed {
    /// For the server list: "↓210 ↑38 Мбит/с".
    pub fn short_label(&self) -> String {
        let n = |v: f64| if v < 10.0 { format!("{v:.1}") } else { format!("{v:.0}") };
        match self.up_mbps {
            Some(up) => format!("↓{} ↑{} Мбит/с", n(self.down_mbps), n(up)),
            None => format!("↓{} Мбит/с", n(self.down_mbps)),
        }
    }
}

/// Measures through xray's SOCKS inbound. `cancel` stops it mid-transfer.
pub fn measure(socks: SocketAddr, user: &str, pass: &str, source: &Source, cancel: &AtomicBool) -> Result<Speed> {
    let proxy = ureq::Proxy::new(&if user.is_empty() { format!("socks5://{socks}") } else { format!("socks5://{user}:{pass}@{socks}") })?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(Some(proxy))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .build()
        .into();
    match source {
        Source::Ookla => ookla(&agent, cancel),
        Source::Cloudflare => {
            let down_mbps = download(&agent, CLOUDFLARE_DOWN, cancel)?;
            // Upload is a nice-to-have: a failure leaves it out.
            let up_mbps = if cancel.load(Ordering::SeqCst) { None } else { upload(&agent).ok() };
            Ok(Speed { down_mbps, up_mbps })
        }
        Source::Own(url) => Ok(Speed { down_mbps: download(&agent, url.trim(), cancel)?, up_mbps: None }),
    }
}

#[derive(Deserialize)]
struct OoklaServer {
    /// "host:port".
    host: String,
}

/// The nearest Speedtest server that answers; the next ones on failure.
fn ookla(agent: &ureq::Agent, cancel: &AtomicBool) -> Result<Speed> {
    let servers: Vec<OoklaServer> = agent.get(OOKLA_SERVERS).call().context("список серверов Speedtest")?.body_mut().read_json()?;
    let mut last = anyhow::anyhow!("Speedtest не дал серверов");
    for server in servers.iter().take(3) {
        if cancel.load(Ordering::SeqCst) {
            bail!("остановлено");
        }
        let base = format!("https://{}", server.host);
        match ookla_download(agent, &base, cancel) {
            Ok(down_mbps) => {
                let up_mbps = if cancel.load(Ordering::SeqCst) { None } else { ookla_upload(agent, &base, cancel).ok() };
                return Ok(Speed { down_mbps, up_mbps });
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Downloads in parallel streams until MAX_TIME.
fn ookla_download(agent: &ureq::Agent, base: &str, cancel: &AtomicBool) -> Result<f64> {
    let total = AtomicUsize::new(0);
    let start = Instant::now();
    let running = || start.elapsed() < MAX_TIME && !cancel.load(Ordering::SeqCst);
    let errors: Vec<_> = std::thread::scope(|scope| {
        let streams: Vec<_> = (0..OOKLA_STREAMS)
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    let mut buf = vec![0; 64 * 1024];
                    while running() {
                        let url = format!("{base}/download?nocache={}&size={OOKLA_CHUNK}", fastrand::u64(..));
                        let mut resp = agent.get(&url).call()?;
                        let mut body = resp.body_mut().as_reader();
                        while running() {
                            let n = body.read(&mut buf)?;
                            if n == 0 {
                                break;
                            }
                            total.fetch_add(n, Ordering::Relaxed);
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        streams.into_iter().filter_map(|s| s.join().ok()?.err()).collect()
    });
    if cancel.load(Ordering::SeqCst) {
        bail!("остановлено");
    }
    let total = total.into_inner();
    if total < 100_000 {
        return Err(errors.into_iter().next().unwrap_or_else(|| anyhow::anyhow!("получено слишком мало данных"))).context(base.to_string());
    }
    Ok(mbps(total, start.elapsed()))
}

/// Uploads in parallel streams until MAX_TIME. A request cut at the end
/// still counts what it sent.
fn ookla_upload(agent: &ureq::Agent, base: &str, cancel: &AtomicBool) -> Result<f64> {
    let total = AtomicUsize::new(0);
    let start = Instant::now();
    let running = || start.elapsed() < MAX_TIME && !cancel.load(Ordering::SeqCst);
    std::thread::scope(|scope| {
        for _ in 0..OOKLA_STREAMS {
            scope.spawn(|| {
                while running() {
                    let url = format!("{base}/upload?nocache={}", fastrand::u64(..));
                    let mut body = Counted { left: OOKLA_CHUNK, total: &total, running: &running };
                    // The server needs a length: it does not take chunked bodies.
                    let sent = agent
                        .post(&url)
                        .header("Content-Length", OOKLA_CHUNK.to_string())
                        .config()
                        .timeout_global(Some(MAX_TIME + Duration::from_secs(5)))
                        .build()
                        .send(ureq::SendBody::from_reader(&mut body));
                    if sent.is_err() {
                        break;
                    }
                }
            });
        }
    });
    if cancel.load(Ordering::SeqCst) {
        bail!("остановлено");
    }
    let total = total.into_inner();
    if total < 100_000 {
        bail!("отдача не удалась");
    }
    Ok(mbps(total, start.elapsed()))
}

/// Zeros for an upload body, counted as they go out; ends the request with an
/// error once time is up.
struct Counted<'a, F: Fn() -> bool> {
    left: usize,
    total: &'a AtomicUsize,
    running: &'a F,
}

impl<F: Fn() -> bool> Read for Counted<'_, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !(self.running)() {
            return Err(std::io::Error::other("time is up"));
        }
        let n = buf.len().min(self.left);
        buf[..n].fill(0);
        self.left -= n;
        self.total.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}

fn download(agent: &ureq::Agent, url: &str, cancel: &AtomicBool) -> Result<f64> {
    let mut resp = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    let mut body = resp.body_mut().as_reader();
    let mut buf = vec![0; 64 * 1024];
    let (start, mut total) = (Instant::now(), 0usize);
    while start.elapsed() < MAX_TIME {
        if cancel.load(Ordering::SeqCst) {
            bail!("остановлено");
        }
        let n = body.read(&mut buf).context("загрузка оборвалась")?;
        if n == 0 {
            break;
        }
        total += n;
    }
    if total < 100_000 {
        bail!("получено слишком мало данных ({total} байт): проверьте адрес файла");
    }
    Ok(mbps(total, start.elapsed()))
}

fn upload(agent: &ureq::Agent) -> Result<f64> {
    let data = vec![0u8; UPLOAD_BYTES];
    let start = Instant::now();
    agent
        .post(CLOUDFLARE_UP)
        .config()
        .timeout_global(Some(MAX_TIME + Duration::from_secs(5)))
        .build()
        .send(&data[..])
        .context("отдача")?;
    Ok(mbps(UPLOAD_BYTES, start.elapsed()))
}

fn mbps(bytes: usize, took: Duration) -> f64 {
    bytes as f64 * 8.0 / took.as_secs_f64().max(0.001) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through a running SOCKS proxy (network): DUORAY_SPEED_SOCKS=127.0.0.1:1080 cargo test -- --ignored
    #[test]
    #[ignore]
    fn measures_through_a_proxy() {
        let socks = std::env::var("DUORAY_SPEED_SOCKS").expect("DUORAY_SPEED_SOCKS").parse().unwrap();
        for source in [Source::Ookla, Source::Cloudflare] {
            let speed = measure(socks, "", "", &source, &AtomicBool::new(false)).unwrap();
            eprintln!("{}", speed.short_label());
            assert!(speed.down_mbps > 0.0 && speed.up_mbps.is_some());
        }
    }

    #[test]
    fn labels_and_math() {
        assert!((mbps(12_500_000, Duration::from_secs(1)) - 100.0).abs() < 1e-9);
        assert_eq!(Speed { down_mbps: 5.0, up_mbps: None }.short_label(), "↓5.0 Мбит/с");
        assert_eq!(Speed { down_mbps: 209.6, up_mbps: Some(8.24) }.short_label(), "↓210 ↑8.2 Мбит/с");
    }
}

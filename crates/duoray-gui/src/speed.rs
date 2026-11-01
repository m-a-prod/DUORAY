//! Speed test of a server: download (and, from Cloudflare, upload) through
//! it, in megabits a second.

use std::io::Read;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Cloudflare's speed test endpoints, used when no own file is set.
const CLOUDFLARE_DOWN: &str = "https://speed.cloudflare.com/__down?bytes=25000000";
const CLOUDFLARE_UP: &str = "https://speed.cloudflare.com/__up";
/// A test stops after this long, or once the file is in.
const MAX_TIME: Duration = Duration::from_secs(10);
const UPLOAD_BYTES: usize = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speed {
    pub down_mbps: f64,
    /// Only measured against Cloudflare.
    pub up_mbps: Option<f64>,
}

impl Speed {
    /// "↓ 85.3 Мбит/с · ↑ 21.0 Мбит/с"
    pub fn label(&self) -> String {
        let down = format!("↓ {:.1} Мбит/с", self.down_mbps);
        match self.up_mbps {
            Some(up) => format!("{down} · ↑ {up:.1} Мбит/с"),
            None => down,
        }
    }
}

/// Measures through xray's SOCKS inbound. `url` is an own file to download,
/// or empty for Cloudflare.
pub fn measure(socks: SocketAddr, user: &str, pass: &str, url: &str) -> Result<Speed> {
    let proxy = ureq::Proxy::new(&if user.is_empty() { format!("socks5://{socks}") } else { format!("socks5://{user}:{pass}@{socks}") })?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(Some(proxy))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .build()
        .into();
    let cloudflare = url.trim().is_empty();
    let down_mbps = download(&agent, if cloudflare { CLOUDFLARE_DOWN } else { url.trim() })?;
    // Upload is a nice-to-have: a failure leaves it out.
    let up_mbps = if cloudflare { upload(&agent).ok() } else { None };
    Ok(Speed { down_mbps, up_mbps })
}

fn download(agent: &ureq::Agent, url: &str) -> Result<f64> {
    let mut resp = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    let mut body = resp.body_mut().as_reader();
    let mut buf = vec![0; 64 * 1024];
    let (start, mut total) = (Instant::now(), 0usize);
    while start.elapsed() < MAX_TIME {
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
        let speed = measure(socks, "", "", "").unwrap();
        eprintln!("{}", speed.label());
        assert!(speed.down_mbps > 0.0 && speed.up_mbps.is_some());
    }

    #[test]
    fn labels_and_math() {
        assert!((mbps(12_500_000, Duration::from_secs(1)) - 100.0).abs() < 1e-9);
        assert_eq!(Speed { down_mbps: 85.34, up_mbps: Some(21.0) }.label(), "↓ 85.3 Мбит/с · ↑ 21.0 Мбит/с");
        assert_eq!(Speed { down_mbps: 5.0, up_mbps: None }.label(), "↓ 5.0 Мбит/с");
    }
}

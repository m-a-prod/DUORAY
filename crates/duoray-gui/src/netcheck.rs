//! "Диагностика сети": the network step by step, from the default route to
//! the VPN's exit, for the user and for support (the result copies as text).

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use duoray_core::balancer;
use duoray_core::store::ProxyPingMode;

use crate::{connection, exit, ping};

/// A check's outcome, as the dialog colours it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Running = 0,
    Ok = 1,
    Warn = 2,
    Fail = 3,
    /// Nothing to check (no subscription, not connected).
    Skip = 4,
}

pub const TITLES: [&str; 8] = ["Сеть", "DNS", "Интернет", "Часы", "Сервер подписки", "Выбранный сервер", "Через VPN", "Другие VPN"];

/// What to check, taken from the app before the run.
pub struct Input {
    /// The current subscription's URL.
    pub subscription: Option<String>,
    /// The selected server: (address, port, reachable over TCP).
    pub server: Option<(String, u16, bool)>,
    /// xray's SOCKS inbound (address, user, password) while connected.
    pub vpn: Option<(SocketAddr, String, String)>,
    pub ping_url: String,
}

const TIMEOUT: Duration = Duration::from_secs(5);

/// Runs the checks one after another, telling `report` (index into TITLES,
/// level, detail) as each ends.
pub fn run(input: Input, report: impl Fn(usize, Level, String)) {
    let ms = |since: Instant| since.elapsed().as_millis();

    // 0 Сеть
    match connection::uplink_interface() {
        Some(dev) => report(0, Level::Ok, format!("подключение через {dev}")),
        None => report(0, Level::Fail, "нет маршрута по умолчанию: компьютер не подключён к сети".into()),
    }

    // 1 DNS
    let start = Instant::now();
    match ("www.google.com", 443).to_socket_addrs() {
        Ok(_) => report(1, Level::Ok, format!("имена находятся, {} мс", ms(start))),
        Err(e) => report(1, Level::Fail, format!("имена не находятся: {e}")),
    }

    // 2 Интернет, 3 Часы (by the answer's Date)
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).http_status_as_error(false).max_redirects(0).build().into();
    let via = if input.vpn.is_some() { " (через VPN)" } else { "" };
    let start = Instant::now();
    let google = agent.get("http://www.gstatic.com/generate_204").call();
    let took = ms(start);
    let date = match &google {
        Ok(resp) => {
            report(2, Level::Ok, format!("Google отвечает за {took} мс{via}"));
            resp.headers().get("date").and_then(|d| d.to_str().ok()).map(str::to_string)
        }
        Err(_) => {
            match agent.get("http://ya.ru/").call() {
                Ok(_) => report(2, Level::Warn, format!("Google не отвечает, Яндекс отвечает{via}")),
                Err(e) => report(2, Level::Fail, format!("сайты не отвечают{via}: {e}")),
            }
            None
        }
    };
    match date.as_deref().and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok()) {
        Some(server) => {
            let skew = (chrono::Utc::now() - server.to_utc()).num_seconds();
            let (level, detail) = clock(skew);
            report(3, level, detail);
        }
        None => report(3, Level::Skip, "не с чем сравнить: интернет не ответил".into()),
    }

    // 4 Сервер подписки
    match input.subscription.as_deref().and_then(|u| url::Url::parse(u).ok()) {
        Some(url) => {
            let host = url.host_str().unwrap_or_default().to_string();
            let port = url.port_or_known_default().unwrap_or(443);
            match tcp(&host, port) {
                Ok(took) => report(4, Level::Ok, format!("{host} отвечает за {took} мс{via}")),
                Err(e) => report(4, Level::Fail, format!("{host} не отвечает: {e}")),
            }
        }
        None => report(4, Level::Skip, "подписка не выбрана".into()),
    }

    // 5 Выбранный сервер (straight: its address bypasses the tunnel)
    match &input.server {
        Some((host, port, true)) => match tcp(host, *port) {
            Ok(took) => report(5, Level::Ok, format!("порт {port} открыт, {took} мс")),
            Err(e) => report(5, Level::Fail, format!("порт {port} не отвечает: {e}")),
        },
        Some((_, _, false)) => report(5, Level::Skip, "работает по UDP: проверяется подключением".into()),
        None => report(5, Level::Skip, "сервер не выбран или это балансировщик".into()),
    }

    // 6 Через VPN
    match &input.vpn {
        Some((socks, user, pass)) => {
            match ping::via_socks_mode(*socks, user, pass, &input.ping_url, false, TIMEOUT, ProxyPingMode::Default) {
                Ok(took) => {
                    let exit = exit::trace(*socks, user, pass)
                        .map(|t| format!(", выход: {} ({})", balancer::country_name(&t.loc).unwrap_or(&t.loc), t.ip))
                        .unwrap_or_default();
                    report(6, Level::Ok, format!("отвечает за {} мс{exit}", took.as_millis()));
                }
                Err(e) => report(6, Level::Fail, format!("сервер не пропускает трафик: {e:#}")),
            }
        }
        None => report(6, Level::Skip, "VPN не подключён".into()),
    }

    // 7 Другие VPN
    let others = connection::other_vpn_processes();
    if others.is_empty() {
        report(7, Level::Ok, "не запущены".into());
    } else {
        report(7, Level::Warn, format!("запущены {}: могут перехватывать трафик или останавливать xray", others.join(", ")));
    }
}

/// The system clock against a server's, `skew` seconds ahead (negative: behind).
fn clock(skew: i64) -> (Level, String) {
    let ahead = if skew > 0 { "спешат" } else { "отстают" };
    match skew.abs() {
        0..60 => (Level::Ok, "точные".into()),
        60..600 => (Level::Warn, format!("{ahead} на {} с", skew.abs())),
        s => (
            Level::Fail,
            format!("{ahead} на {} мин: защищённые подключения могут не устанавливаться. Включите синхронизацию времени", s / 60),
        ),
    }
}

/// TCP connect time to `host:port`, any of its addresses.
fn tcp(host: &str, port: u16) -> Result<u128, String> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| format!("адрес не находится: {e}"))?.collect();
    let mut last = "нет адресов".to_string();
    for addr in addrs {
        let start = Instant::now();
        match TcpStream::connect_timeout(&addr, TIMEOUT) {
            Ok(_) => return Ok(start.elapsed().as_millis()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// The result as text for support: one line per check.
pub fn as_text(rows: &[(Level, String, String)]) -> String {
    let mark = |l: Level| match l {
        Level::Ok => "✓",
        Level::Warn => "!",
        Level::Fail => "✗",
        Level::Skip => "–",
        Level::Running => "…",
    };
    let lines: Vec<String> = rows.iter().map(|(l, title, detail)| format!("{} {title}: {detail}", mark(*l))).collect();
    format!("Диагностика сети DUORAY {}\n{}", env!("CARGO_PKG_VERSION"), lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_skew_levels() {
        assert_eq!(clock(5).0, Level::Ok);
        assert_eq!(clock(-120), (Level::Warn, "отстают на 120 с".into()));
        assert!(clock(3600).1.starts_with("спешат на 60 мин"));
        assert_eq!(clock(3600).0, Level::Fail);
    }

    #[test]
    fn text_for_support() {
        let rows = [(Level::Ok, "Сеть".to_string(), "подключение через wlan0".to_string()), (Level::Fail, "DNS".into(), "нет".into())];
        let text = as_text(&rows);
        assert!(text.contains("✓ Сеть: подключение через wlan0\n✗ DNS: нет"));
    }
}

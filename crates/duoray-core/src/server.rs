//! A server entry of a subscription: either a full xray JSON config (the
//! preferred form, it carries the panel's routing, DNS and balancers) or a
//! share link parsed into a [`Profile`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::link::Profile;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub description: Option<String>,
    /// Display label: VLESS, Hysteria2, ...
    pub protocol: String,
    pub address: String,
    pub port: u16,
    pub network: String,
    pub security: String,
    /// Number of proxy outbounds; >1 means a balancer ("auto-select").
    pub proxies: usize,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum Source {
    Json(Value),
    Link(Box<Profile>),
}

/// Outbound protocols that do not lead to a server.
const SERVICE_PROTOCOLS: &[&str] = &["freedom", "blackhole", "dns", "loopback"];

fn protocol_label(p: &str) -> String {
    match p {
        "vless" => "VLESS".into(),
        "vmess" => "VMess".into(),
        "trojan" => "Trojan".into(),
        "shadowsocks" => "Shadowsocks".into(),
        // xray's `hysteria` outbound speaks Hysteria 2.
        "hysteria" | "hysteria2" => "Hysteria2".into(),
        "wireguard" => "WireGuard".into(),
        "socks" => "SOCKS".into(),
        "http" => "HTTP".into(),
        other => other.to_string(),
    }
}

impl Server {
    /// Summarizes one xray config from a JSON subscription. `None` if it has
    /// no usable proxy outbound.
    pub fn from_xray_config(config: Value) -> Option<Self> {
        let outbounds = config.get("outbounds")?.as_array()?;
        let proxies: Vec<&Value> = outbounds
            .iter()
            .filter(|o| {
                let p = o["protocol"].as_str().unwrap_or_default();
                !p.is_empty() && !SERVICE_PROTOCOLS.contains(&p)
            })
            .collect();
        let main = proxies
            .iter()
            .find(|o| o["tag"].as_str() == Some("proxy"))
            .or_else(|| proxies.first())?;

        let settings = &main["settings"];
        // vnext (vless/vmess), servers (trojan/ss/socks), or the flat form newer xray accepts.
        let endpoint = settings["vnext"]
            .get(0)
            .or_else(|| settings["servers"].get(0))
            .unwrap_or(settings);
        let address = endpoint["address"].as_str().unwrap_or_default().to_string();
        let port = endpoint["port"].as_u64().unwrap_or(0) as u16;

        let stream = &main["streamSettings"];
        let network = match stream["network"].as_str() {
            Some("raw") | None => "tcp",
            Some("splithttp") => "xhttp",
            Some(n) => n,
        }
        .to_string();
        let security = stream["security"].as_str().unwrap_or("none").to_string();

        let name = config["remarks"]
            .as_str()
            .or_else(|| config["remark"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "null")
            .map(String::from)
            .unwrap_or_else(|| format!("{address}:{port}"));
        let description = config["meta"]["serverDescription"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        Some(Self {
            name,
            description,
            protocol: protocol_label(main["protocol"].as_str().unwrap_or_default()),
            address,
            port,
            network,
            security,
            proxies: proxies.len(),
            source: Source::Json(config),
        })
    }

    pub fn from_link(p: Profile) -> Self {
        Self {
            name: p.name.clone(),
            description: p.description.clone(),
            protocol: p.protocol.label().to_string(),
            address: p.address.clone(),
            port: p.port,
            network: p.stream.network.clone(),
            security: p.stream.security.clone(),
            proxies: 1,
            source: Source::Link(Box::new(p)),
        }
    }
}

/// Splits a leading flag emoji off a name: "🇸🇪 Швеция" -> (Some("SE"), "Швеция").
/// Other emoji are dropped from the display name; they render inconsistently
/// across platforms and GPU backends.
pub fn display_name(name: &str) -> (Option<String>, String) {
    let chars: Vec<char> = name.trim().chars().collect();
    let regional = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
    let flag = match chars.as_slice() {
        [a, b, ..] if regional(*a) && regional(*b) => Some(
            [*a, *b]
                .iter()
                .map(|c| (b'A' + (*c as u32 - 0x1F1E6) as u8) as char)
                .collect::<String>(),
        ),
        _ => None,
    };
    let is_emoji = |c: char| {
        let u = c as u32;
        (0x1F000..=0x1FAFF).contains(&u)
            || (0x2600..=0x27BF).contains(&u)
            || (0x2B00..=0x2BFF).contains(&u)
            || c == '\u{FE0F}'
            || c == '\u{200D}'
    };
    let rest: String = chars.into_iter().filter(|c| !is_emoji(*c)).collect();
    let rest = rest.split_whitespace().collect::<Vec<_>>().join(" ");
    (flag, rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarizes_balancer_config() {
        let cfg = json!({
            "remarks": " 🇪🇺🛜 Wi-Fi • Авто-выбор",
            "outbounds": [
                {"tag": "proxy", "protocol": "vless",
                 "settings": {"vnext": [{"address": "5.6.7.8", "port": 443, "users": [{"id": "x"}]}]},
                 "streamSettings": {"network": "xhttp", "security": "reality"}},
                {"tag": "proxy-2", "protocol": "vless", "settings": {"vnext": [{"address": "9.9.9.9", "port": 443}]}},
                {"tag": "direct", "protocol": "freedom"},
                {"tag": "block", "protocol": "blackhole"}
            ]
        });
        let s = Server::from_xray_config(cfg).unwrap();
        assert_eq!(s.name, "🇪🇺🛜 Wi-Fi • Авто-выбор");
        assert_eq!((s.protocol.as_str(), s.address.as_str(), s.port), ("VLESS", "5.6.7.8", 443));
        assert_eq!((s.network.as_str(), s.security.as_str(), s.proxies), ("xhttp", "reality", 2));
    }

    #[test]
    fn hysteria_and_meta() {
        let cfg = json!({
            "remarks": "🇪🇪 LTE #1", "meta": {"serverDescription": "Безлимитный"},
            "outbounds": [{"tag": "proxy", "protocol": "hysteria",
                "settings": {"address": "37.0.0.1", "port": 45443},
                "streamSettings": {"network": "hysteria", "security": "tls"}}]
        });
        let s = Server::from_xray_config(cfg).unwrap();
        assert_eq!((s.protocol.as_str(), s.port), ("Hysteria2", 45443));
        assert_eq!(s.description.as_deref(), Some("Безлимитный"));
    }

    #[test]
    fn no_proxy_outbound() {
        assert!(Server::from_xray_config(json!({"outbounds": [{"protocol": "freedom"}]})).is_none());
    }

    #[test]
    fn flags() {
        assert_eq!(display_name("🇸🇪 Швеция (Фалькенберг 0x)"), (Some("SE".into()), "Швеция (Фалькенберг 0x)".into()));
        assert_eq!(display_name(" 🇪🇺🛜 Wi-Fi • Авто-выбор"), (Some("EU".into()), "Wi-Fi • Авто-выбор".into()));
        assert_eq!(display_name("Plain"), (None, "Plain".into()));
    }
}

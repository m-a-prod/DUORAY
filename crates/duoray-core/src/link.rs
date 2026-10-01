//! Share-link parsing: vless://, vmess://, trojan://, ss://.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_NO_PAD_INDIFFERENT, URL_SAFE_NO_PAD_INDIFFERENT};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// Remnawave `serverDescription` (shown instead of the protocol name).
    #[serde(default)]
    pub description: Option<String>,
    pub protocol: Protocol,
    pub address: String,
    pub port: u16,
    pub stream: Stream,
    /// The original link, kept for sharing/export.
    pub link: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Protocol {
    Vless { id: String, flow: String, encryption: String },
    Vmess { id: String, alter_id: u32, cipher: String },
    Trojan { password: String },
    Shadowsocks { method: String, password: String },
}

impl Protocol {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Vless { .. } => "VLESS",
            Self::Vmess { .. } => "VMess",
            Self::Trojan { .. } => "Trojan",
            Self::Shadowsocks { .. } => "Shadowsocks",
        }
    }
}

/// Transport and security settings, in share-link terms.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    /// tcp | ws | grpc | xhttp | httpupgrade | kcp
    pub network: String,
    /// none | tls | reality
    pub security: String,
    pub sni: String,
    pub alpn: Vec<String>,
    pub fingerprint: String,
    pub allow_insecure: bool,
    pub public_key: String,
    pub short_id: String,
    pub spider_x: String,
    pub host: String,
    pub path: String,
    pub service_name: String,
    pub mode: String,
    pub header_type: String,
    /// xhttp `extra` object, passed through verbatim.
    pub extra: Option<serde_json::Value>,
}

pub fn parse(link: &str) -> Result<Profile> {
    let original = link.trim();
    let (link, description) = split_server_description(original);
    let mut p = parse_plain(link)?;
    p.description = description;
    p.link = original.to_string();
    Ok(p)
}

/// Remnawave appends `?serverDescription=<base64>` to the *whole* link, i.e.
/// after the `#name` fragment, and without URL-encoding the base64.
fn split_server_description(link: &str) -> (&str, Option<String>) {
    const MARK: &str = "?serverDescription=";
    let Some(pos) = link.rfind(MARK) else { return (link, None) };
    let raw = &link[pos + MARK.len()..];
    let description = b64(&decode(raw))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (&link[..pos], description)
}

fn parse_plain(link: &str) -> Result<Profile> {
    let scheme = link.split("://").next().unwrap_or_default().to_ascii_lowercase();
    match scheme.as_str() {
        "vless" => parse_vless_trojan(link, true),
        "trojan" => parse_vless_trojan(link, false),
        "vmess" => parse_vmess(link),
        "ss" => parse_ss(link),
        "" => bail!("not a link"),
        other => bail!("unsupported protocol: {other}"),
    }
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn fragment_name(url: &Url, fallback: &str) -> String {
    match url.fragment().map(decode) {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => fallback.to_string(),
    }
}

fn host_port(url: &Url) -> Result<(String, u16)> {
    let host = url.host_str().context("missing host")?;
    // url keeps IPv6 literals bracketed; xray wants them bare.
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = url.port().context("missing port")?;
    Ok((host, port))
}

fn stream_from_query(q: &HashMap<String, String>, default_security: &str) -> Result<Stream> {
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    let extra = match q.get("extra") {
        Some(e) if !e.is_empty() => Some(serde_json::from_str(e).context("bad xhttp extra")?),
        _ => None,
    };
    let network = match get("type").as_str() {
        "" | "raw" => "tcp".to_string(),
        "splithttp" => "xhttp".to_string(),
        n => n.to_string(),
    };
    let security = match get("security").as_str() {
        "" => default_security.to_string(),
        s => s.to_string(),
    };
    Ok(Stream {
        network,
        security,
        sni: get("sni"),
        alpn: get("alpn").split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
        fingerprint: get("fp"),
        allow_insecure: matches!(get("allowInsecure").as_str(), "1" | "true"),
        public_key: get("pbk"),
        short_id: get("sid"),
        spider_x: get("spx"),
        host: get("host"),
        path: get("path"),
        service_name: get("serviceName"),
        mode: get("mode"),
        header_type: get("headerType"),
        extra,
    })
}

fn parse_vless_trojan(link: &str, vless: bool) -> Result<Profile> {
    let url = Url::parse(link)?;
    let (address, port) = host_port(&url)?;
    let secret = decode(url.username());
    if secret.is_empty() {
        bail!("missing {}", if vless { "uuid" } else { "password" });
    }
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let protocol = if vless {
        Protocol::Vless {
            id: secret,
            flow: q.get("flow").cloned().unwrap_or_default(),
            encryption: q.get("encryption").cloned().unwrap_or_else(|| "none".into()),
        }
    } else {
        Protocol::Trojan { password: secret }
    };
    // Trojan is TLS unless stated otherwise.
    let stream = stream_from_query(&q, if vless { "none" } else { "tls" })?;
    Ok(Profile {
        name: fragment_name(&url, &address),
        description: None,
        protocol,
        address,
        port,
        stream,
        link: link.to_string(),
    })
}

fn b64(s: &str) -> Result<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD_NO_PAD_INDIFFERENT
        .decode(&s)
        .or_else(|_| URL_SAFE_NO_PAD_INDIFFERENT.decode(&s))
        .map_err(|e| anyhow!("bad base64: {e}"))
}

fn parse_vmess(link: &str) -> Result<Profile> {
    let body = &link["vmess://".len()..];
    let json: serde_json::Value = serde_json::from_slice(&b64(body)?).context("vmess: bad json")?;
    // Fields are strings or numbers depending on the generator.
    let s = |k: &str| match &json[k] {
        serde_json::Value::String(v) => v.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let address = s("add");
    if address.is_empty() {
        bail!("vmess: missing address");
    }
    let port: u16 = s("port").parse().context("vmess: bad port")?;
    let mut q: HashMap<String, String> = HashMap::new();
    for (from, to) in [
        ("net", "type"),
        ("tls", "security"),
        ("sni", "sni"),
        ("alpn", "alpn"),
        ("fp", "fp"),
        ("host", "host"),
        ("path", "path"),
        ("type", "headerType"),
    ] {
        let v = s(from);
        if !v.is_empty() {
            q.insert(to.into(), v);
        }
    }
    let mut stream = stream_from_query(&q, "none")?;
    if stream.network == "grpc" {
        // vmess links carry the gRPC service name in `path`.
        stream.service_name = std::mem::take(&mut stream.path);
    }
    let name = match s("ps") {
        n if n.trim().is_empty() => address.clone(),
        n => n.trim().to_string(),
    };
    Ok(Profile {
        name,
        description: None,
        protocol: Protocol::Vmess {
            id: s("id"),
            alter_id: s("aid").parse().unwrap_or(0),
            cipher: match s("scy") {
                c if c.is_empty() => "auto".into(),
                c => c,
            },
        },
        address,
        port,
        stream,
        link: link.to_string(),
    })
}

fn parse_ss(link: &str) -> Result<Profile> {
    let (main, name) = match link.split_once('#') {
        Some((m, n)) => (m, Some(decode(n))),
        None => (link, None),
    };
    let main = main.split('?').next().unwrap_or(main);
    let body = &main["ss://".len()..];

    // SIP002: base64(method:password)@host:port ; legacy: base64(method:password@host:port)
    let (userinfo, hostport) = match body.rsplit_once('@') {
        Some((u, h)) => {
            let u = decode(u);
            let creds = match b64(&u) {
                Ok(b) => String::from_utf8(b).context("ss: bad userinfo")?,
                Err(_) => u, // plain method:password (2022 ciphers)
            };
            (creds, h.to_string())
        }
        None => {
            let full = String::from_utf8(b64(body)?).context("ss: bad body")?;
            let (u, h) = full.rsplit_once('@').context("ss: missing host")?;
            (u.to_string(), h.to_string())
        }
    };
    let (method, password) = userinfo.split_once(':').context("ss: missing method")?;
    let url = Url::parse(&format!("ss://x@{}", hostport.trim_end_matches('/')))?;
    let (address, port) = host_port(&url)?;
    Ok(Profile {
        name: name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| address.clone()),
        description: None,
        protocol: Protocol::Shadowsocks {
            method: method.to_string(),
            password: password.to_string(),
        },
        address,
        port,
        stream: Stream {
            network: "tcp".into(),
            security: "none".into(),
            ..Default::default()
        },
        link: link.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "00000000-1111-2222-3333-444444444444";

    #[test]
    fn vless_reality_xhttp() {
        let p = parse(&format!(
            "vless://{UUID}@188.0.2.1:443?encryption=none&type=xhttp&security=reality&sni=api.example.ru&fp=edge&pbk=PUBKEY&sid=63&spx=%2F&host=api.example.ru&path=%2F&mode=auto#%F0%9F%87%B8%F0%9F%87%AA%20LTE%20%232%20(1x)"
        ))
        .unwrap();
        assert_eq!(p.name, "🇸🇪 LTE #2 (1x)");
        assert_eq!((p.address.as_str(), p.port), ("188.0.2.1", 443));
        assert_eq!(p.stream.network, "xhttp");
        assert_eq!(p.stream.security, "reality");
        assert_eq!(p.stream.public_key, "PUBKEY");
        assert_eq!(p.stream.spider_x, "/");
        assert!(matches!(p.protocol, Protocol::Vless { ref id, .. } if id == UUID));
    }

    #[test]
    fn vless_tls_alpn() {
        let p = parse(&format!(
            "vless://{UUID}@se.example.com:443?type=xhttp&security=tls&sni=se.example.com&alpn=h2,http/1.1&fp=edge&path=/&mode=auto#SE"
        ))
        .unwrap();
        assert_eq!(p.stream.alpn, vec!["h2", "http/1.1"]);
        assert_eq!(p.name, "SE");
    }

    #[test]
    fn vmess() {
        let json = serde_json::json!({
            "v": "2", "ps": "VM", "add": "1.2.3.4", "port": 8443, "id": UUID,
            "aid": "0", "net": "ws", "host": "h.example", "path": "/ws", "tls": "tls", "sni": "h.example"
        });
        let link = format!("vmess://{}", base64::engine::general_purpose::STANDARD.encode(json.to_string()));
        let p = parse(&link).unwrap();
        assert_eq!((p.name.as_str(), p.port), ("VM", 8443));
        assert_eq!((p.stream.network.as_str(), p.stream.security.as_str()), ("ws", "tls"));
        assert_eq!(p.stream.path, "/ws");
    }

    #[test]
    fn trojan_defaults_to_tls() {
        let p = parse("trojan://secret@t.example:443?sni=t.example#T").unwrap();
        assert_eq!(p.stream.security, "tls");
        assert!(matches!(p.protocol, Protocol::Trojan { ref password } if password == "secret"));
    }

    #[test]
    fn shadowsocks_sip002_and_legacy() {
        let creds = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("aes-256-gcm:pa:ss");
        let p = parse(&format!("ss://{creds}@5.6.7.8:8388#SS")).unwrap();
        assert!(
            matches!(p.protocol, Protocol::Shadowsocks { ref method, ref password } if method == "aes-256-gcm" && password == "pa:ss")
        );
        assert_eq!((p.address.as_str(), p.port), ("5.6.7.8", 8388));

        let legacy = base64::engine::general_purpose::STANDARD.encode("chacha20-ietf-poly1305:pw@9.9.9.9:1234");
        let p = parse(&format!("ss://{legacy}#Old")).unwrap();
        assert_eq!((p.address.as_str(), p.port, p.name.as_str()), ("9.9.9.9", 1234, "Old"));
    }

    #[test]
    fn server_description() {
        let d = base64::engine::general_purpose::STANDARD.encode("Все кроме Мегафон, Безлимит");
        let p = parse(&format!("vless://{UUID}@1.2.3.4:443?security=tls#%F0%9F%87%B8%F0%9F%87%AA%20X?serverDescription={d}")).unwrap();
        assert_eq!(p.description.as_deref(), Some("Все кроме Мегафон, Безлимит"));
        assert_eq!(p.name, "🇸🇪 X");
        assert_eq!(p.stream.security, "tls");
        assert!(parse("vless://x@h:1#N").unwrap().description.is_none());
    }

    #[test]
    fn unsupported() {
        assert!(parse("hysteria2://x@h:1").is_err());
        assert!(parse("garbage").is_err());
    }
}

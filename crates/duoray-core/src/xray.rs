//! xray config for servers that came as share links (JSON subscriptions
//! already carry a complete config).

use serde_json::{Value, json};

use crate::link::{Profile, Protocol, Stream};

pub fn config_from_profile(p: &Profile) -> Value {
    json!({
        "log": { "loglevel": "warning" },
        "outbounds": [
            outbound(p, "proxy"),
            { "tag": "direct", "protocol": "freedom" },
            { "tag": "block", "protocol": "blackhole" }
        ],
        "routing": {
            "rules": [
                { "type": "field", "ip": ["geoip:private"], "outboundTag": "direct" }
            ]
        }
    })
}

pub fn outbound(p: &Profile, tag: &str) -> Value {
    let (protocol, settings) = match &p.protocol {
        Protocol::Vless { id, flow, encryption } => {
            let mut user = json!({ "id": id, "encryption": encryption });
            if !flow.is_empty() {
                user["flow"] = json!(flow);
            }
            ("vless", json!({ "vnext": [{ "address": p.address, "port": p.port, "users": [user] }] }))
        }
        Protocol::Vmess { id, alter_id, cipher } => (
            "vmess",
            json!({ "vnext": [{ "address": p.address, "port": p.port,
                "users": [{ "id": id, "alterId": alter_id, "security": cipher }] }] }),
        ),
        Protocol::Trojan { password } => (
            "trojan",
            json!({ "servers": [{ "address": p.address, "port": p.port, "password": password }] }),
        ),
        Protocol::Shadowsocks { method, password } => (
            "shadowsocks",
            json!({ "servers": [{ "address": p.address, "port": p.port, "method": method, "password": password }] }),
        ),
    };
    json!({
        "tag": tag,
        "protocol": protocol,
        "settings": settings,
        "streamSettings": stream_settings(&p.stream, &p.address),
    })
}

fn stream_settings(s: &Stream, address: &str) -> Value {
    let network = if s.network.is_empty() { "tcp" } else { s.network.as_str() };
    let security = if s.security.is_empty() { "none" } else { s.security.as_str() };
    let mut ss = json!({ "network": network, "security": security });

    let sni = [&s.sni, &s.host].into_iter().find(|v| !v.is_empty()).map(String::as_str).unwrap_or(address);
    let fp = if s.fingerprint.is_empty() { "chrome" } else { s.fingerprint.as_str() };
    match security {
        "tls" => {
            let mut tls = json!({ "serverName": sni, "fingerprint": fp });
            if !s.alpn.is_empty() {
                tls["alpn"] = json!(s.alpn);
            }
            if s.allow_insecure {
                tls["allowInsecure"] = json!(true);
            }
            ss["tlsSettings"] = tls;
        }
        "reality" => {
            ss["realitySettings"] = json!({
                "serverName": sni, "fingerprint": fp, "publicKey": s.public_key,
                "shortId": s.short_id, "spiderX": s.spider_x,
            });
        }
        _ => {}
    }

    let path = if s.path.is_empty() { "/" } else { s.path.as_str() };
    match network {
        "xhttp" => {
            let mut x = json!({ "host": s.host, "path": path, "mode": if s.mode.is_empty() { "auto" } else { s.mode.as_str() } });
            if let Some(extra) = &s.extra {
                x["extra"] = extra.clone();
            }
            ss["xhttpSettings"] = x;
        }
        "ws" => ss["wsSettings"] = json!({ "host": s.host, "path": path }),
        "httpupgrade" => ss["httpupgradeSettings"] = json!({ "host": s.host, "path": path }),
        "grpc" => {
            ss["grpcSettings"] = json!({ "serviceName": s.service_name, "multiMode": s.mode == "multi" })
        }
        "tcp" if s.header_type == "http" => {
            ss["tcpSettings"] = json!({ "header": { "type": "http",
                "request": { "path": [path], "headers": { "Host": [s.host] } } } })
        }
        _ => {}
    }
    ss
}

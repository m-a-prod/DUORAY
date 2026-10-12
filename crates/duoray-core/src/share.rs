//! Share links (`vless://`, `vmess://`, `trojan://`, `ss://`) for a server, to
//! copy into another client. Link servers give back their original link; JSON
//! servers get one built from each proxy outbound (a balancer has several).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use url::form_urlencoded;

use crate::server::{Server, Source};

const SERVICE_PROTOCOLS: &[&str] = &["freedom", "blackhole", "dns", "loopback"];

/// Links for `server`; empty when none of its outbounds has a link form
/// (Hysteria, WireGuard and the like).
pub fn links(server: &Server) -> Vec<String> {
    match &server.source {
        Source::Link(p) => vec![p.link.clone()],
        Source::Json(config) => {
            let proxies: Vec<&Value> = config["outbounds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| !SERVICE_PROTOCOLS.contains(&o["protocol"].as_str().unwrap_or_default()))
                .collect();
            let several = proxies.len() > 1;
            proxies
                .into_iter()
                .filter_map(|o| {
                    let tag = o["tag"].as_str().unwrap_or_default();
                    let name = if several && !tag.is_empty() { format!("{} · {tag}", server.name) } else { server.name.clone() };
                    outbound_link(o, &name)
                })
                .collect()
        }
    }
}

/// The share link of one xray outbound, named `name`.
pub fn outbound_link(ob: &Value, name: &str) -> Option<String> {
    let protocol = ob["protocol"].as_str()?;
    let settings = &ob["settings"];
    // vnext (vless/vmess), servers (trojan/ss), or the flat form newer xray accepts.
    let endpoint = settings["vnext"].get(0).or_else(|| settings["servers"].get(0)).unwrap_or(settings);
    let user = endpoint["users"].get(0).unwrap_or(endpoint);
    let address = endpoint["address"].as_str().filter(|a| !a.is_empty())?;
    let port = endpoint["port"].as_u64()?;
    let host = if address.contains(':') { format!("[{address}]") } else { address.to_string() };
    let fragment = form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>().replace('+', "%20");
    let stream = Stream::of(&ob["streamSettings"]);
    match protocol {
        "vless" => {
            let id = user["id"].as_str()?;
            let mut q = vec![("encryption", user["encryption"].as_str().unwrap_or("none").to_string())];
            push(&mut q, "flow", user["flow"].as_str().unwrap_or_default());
            stream.query(&mut q);
            Some(format!("vless://{id}@{host}:{port}?{}#{fragment}", encode(&q)))
        }
        "trojan" => {
            let password = user["password"].as_str()?;
            let mut q = vec![];
            stream.query(&mut q);
            // Userinfo: a space is %20 there ("+" stays literal).
            let password = form_urlencoded::byte_serialize(password.as_bytes()).collect::<String>().replace('+', "%20");
            Some(format!("trojan://{password}@{host}:{port}?{}#{fragment}", encode(&q)))
        }
        "shadowsocks" => {
            let method = user["method"].as_str()?;
            let password = user["password"].as_str()?;
            let userinfo = URL_SAFE_NO_PAD.encode(format!("{method}:{password}"));
            Some(format!("ss://{userinfo}@{host}:{port}#{fragment}"))
        }
        "vmess" => {
            let v = json!({
                "v": "2", "ps": name, "add": address, "port": port.to_string(),
                "id": user["id"].as_str()?, "aid": user["alterId"].as_u64().unwrap_or(0).to_string(),
                "scy": user["security"].as_str().unwrap_or("auto"),
                "net": stream.network, "type": stream.header_type, "host": stream.host, "path": stream.path,
                "tls": if stream.security == "none" { "" } else { stream.security.as_str() },
                "sni": stream.sni, "alpn": stream.alpn, "fp": stream.fingerprint,
            });
            Some(format!("vmess://{}", STANDARD.encode(v.to_string())))
        }
        _ => None,
    }
}

/// The parts of `streamSettings` that share links carry.
#[derive(Default)]
struct Stream {
    network: String,
    security: String,
    sni: String,
    alpn: String,
    fingerprint: String,
    public_key: String,
    short_id: String,
    spider_x: String,
    host: String,
    path: String,
    service_name: String,
    mode: String,
    header_type: String,
    extra: Option<Value>,
}

impl Stream {
    fn of(ss: &Value) -> Self {
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let network = match ss["network"].as_str().unwrap_or("tcp") {
            "raw" => "tcp".to_string(),
            "splithttp" => "xhttp".to_string(),
            n => n.to_string(),
        };
        let security = ss["security"].as_str().unwrap_or("none").to_string();
        let sec = if security == "reality" { &ss["realitySettings"] } else { &ss["tlsSettings"] };
        let transport = match network.as_str() {
            "ws" => &ss["wsSettings"],
            "xhttp" => {
                if ss["xhttpSettings"].is_object() {
                    &ss["xhttpSettings"]
                } else {
                    &ss["splithttpSettings"]
                }
            }
            "httpupgrade" => &ss["httpupgradeSettings"],
            "grpc" => &ss["grpcSettings"],
            "kcp" => &ss["kcpSettings"],
            _ => &ss["tcpSettings"],
        };
        let host = transport["host"]
            .as_str()
            .or_else(|| transport["headers"]["Host"].as_str())
            .or_else(|| transport["header"]["request"]["headers"]["Host"][0].as_str())
            .unwrap_or_default()
            .to_string();
        let path = transport["path"]
            .as_str()
            .or_else(|| transport["header"]["request"]["path"][0].as_str())
            .unwrap_or_default()
            .to_string();
        let alpn = sec["alpn"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","));
        Stream {
            security,
            sni: s(&sec["serverName"]),
            alpn: alpn.unwrap_or_default(),
            fingerprint: s(&sec["fingerprint"]),
            public_key: s(&sec["publicKey"]),
            short_id: s(&sec["shortId"]),
            spider_x: s(&sec["spiderX"]),
            host,
            path,
            service_name: s(&transport["serviceName"]),
            mode: s(&transport["mode"]),
            header_type: s(&transport["header"]["type"]),
            extra: transport.get("extra").filter(|e| e.is_object()).cloned(),
            network,
        }
    }

    fn query(&self, q: &mut Vec<(&'static str, String)>) {
        q.push(("type", self.network.clone()));
        q.push(("security", self.security.clone()));
        for (key, value) in [
            ("sni", &self.sni),
            ("alpn", &self.alpn),
            ("fp", &self.fingerprint),
            ("pbk", &self.public_key),
            ("sid", &self.short_id),
            ("spx", &self.spider_x),
            ("host", &self.host),
            ("path", &self.path),
            ("serviceName", &self.service_name),
            ("mode", &self.mode),
        ] {
            push(q, key, value);
        }
        if self.header_type != "none" {
            push(q, "headerType", &self.header_type);
        }
        if let Some(extra) = &self.extra {
            q.push(("extra", extra.to_string()));
        }
    }
}

fn push(q: &mut Vec<(&'static str, String)>, key: &'static str, value: &str) {
    if !value.is_empty() {
        q.push((key, value.to_string()));
    }
}

fn encode(q: &[(&str, String)]) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    for (k, v) in q {
        s.append_pair(k, v);
    }
    s.finish().replace('+', "%20")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link;

    fn vless_reality() -> Value {
        json!({"tag": "proxy", "protocol": "vless",
            "settings": {"vnext": [{"address": "de.example.com", "port": 443,
                "users": [{"id": "11111111-2222-3333-4444-555555555555", "flow": "xtls-rprx-vision", "encryption": "none"}]}]},
            "streamSettings": {"network": "xhttp", "security": "reality",
                "realitySettings": {"serverName": "www.example.org", "fingerprint": "chrome", "publicKey": "PUBKEY", "shortId": "ab12", "spiderX": "/"},
                "xhttpSettings": {"path": "/x", "host": "cdn.example.org", "mode": "auto"}}})
    }

    #[test]
    fn vless_link_parses_back_the_same() {
        let url = outbound_link(&vless_reality(), "🇩🇪 Германия").unwrap();
        assert!(url.starts_with("vless://11111111-2222-3333-4444-555555555555@de.example.com:443?"));
        let p = link::parse(&url).unwrap();
        assert_eq!(p.name, "🇩🇪 Германия");
        assert_eq!((p.address.as_str(), p.port), ("de.example.com", 443));
        assert!(matches!(&p.protocol, link::Protocol::Vless { flow, .. } if flow == "xtls-rprx-vision"));
        let s = &p.stream;
        assert_eq!((s.network.as_str(), s.security.as_str()), ("xhttp", "reality"));
        assert_eq!((s.sni.as_str(), s.fingerprint.as_str(), s.public_key.as_str()), ("www.example.org", "chrome", "PUBKEY"));
        assert_eq!((s.short_id.as_str(), s.path.as_str(), s.host.as_str(), s.mode.as_str()), ("ab12", "/x", "cdn.example.org", "auto"));
    }

    #[test]
    fn trojan_ss_vmess_and_balancers() {
        let trojan = json!({"protocol": "trojan", "settings": {"servers": [{"address": "t.example", "port": 8443, "password": "p@ss word"}]},
            "streamSettings": {"network": "ws", "security": "tls", "tlsSettings": {"serverName": "t.example"}, "wsSettings": {"path": "/ws"}}});
        let p = link::parse(&outbound_link(&trojan, "T").unwrap()).unwrap();
        assert!(matches!(&p.protocol, link::Protocol::Trojan { password } if password == "p@ss word"));
        assert_eq!(p.stream.path, "/ws");

        let ss = json!({"protocol": "shadowsocks", "settings": {"servers": [{"address": "s.example", "port": 8388, "method": "aes-256-gcm", "password": "pw"}]}});
        let p = link::parse(&outbound_link(&ss, "S").unwrap()).unwrap();
        assert!(matches!(&p.protocol, link::Protocol::Shadowsocks { method, password } if method == "aes-256-gcm" && password == "pw"));

        let vmess = json!({"protocol": "vmess", "settings": {"vnext": [{"address": "v.example", "port": 443, "users": [{"id": "u", "alterId": 0}]}]},
            "streamSettings": {"network": "ws", "security": "tls", "wsSettings": {"path": "/v", "headers": {"Host": "h.example"}}}});
        let p = link::parse(&outbound_link(&vmess, "V").unwrap()).unwrap();
        assert_eq!((p.stream.host.as_str(), p.stream.path.as_str()), ("h.example", "/v"));

        assert!(outbound_link(&json!({"protocol": "hysteria", "settings": {"address": "h", "port": 1}}), "H").is_none());

        let mut second = vless_reality();
        second["tag"] = json!("proxy-2");
        let server = Server::from_xray_config(json!({"remarks": "Авто", "outbounds": [vless_reality(), second, {"protocol": "freedom"}]})).unwrap();
        let all = links(&server);
        assert_eq!(all.len(), 2, "one per proxy outbound, none for freedom");
        assert!(all[1].ends_with("#%D0%90%D0%B2%D1%82%D0%BE%20%C2%B7%20proxy-2"));
    }
}


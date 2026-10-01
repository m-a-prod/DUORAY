//! Turns a subscription entry into the config xray actually runs.
//!
//! The subscription author's config is kept intact (routing, DNS, balancers,
//! observatory, bridges). Only three things change:
//! 1. `inbounds` become one password-protected SOCKS inbound on loopback,
//!    tagged `socks` like the panel's own, so its routing rules still apply;
//! 2. every outbound that dials the network is bound to the physical
//!    interface, otherwise `direct` traffic would loop back into the TUN;
//! 3. server hostnames are resolved now, before the TUN exists (SNI/Host keep
//!    the name), and those IPs are routed around the TUN.

use std::net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::server::{Server, Source};
use crate::xray;

pub struct Runtime {
    pub config: Value,
    pub socks: SocketAddr,
    pub user: String,
    pub pass: String,
    pub bypass: Vec<IpAddr>,
}

const SERVICE_PROTOCOLS: &[&str] = &["freedom", "blackhole", "dns", "loopback"];

pub fn build(server: &Server, iface: &str) -> Result<Runtime> {
    build_with(server, iface, |host| {
        (host, 0)
            .to_socket_addrs()
            .with_context(|| format!("cannot resolve {host}"))?
            .map(|a| a.ip())
            .find(IpAddr::is_ipv4)
            .with_context(|| format!("{host} has no IPv4 address"))
    })
}

pub fn build_with(
    server: &Server,
    iface: &str,
    mut resolve: impl FnMut(&str) -> Result<IpAddr>,
) -> Result<Runtime> {
    let mut config = match &server.source {
        Source::Json(v) => v.clone(),
        Source::Link(p) => xray::config_from_profile(p),
    };

    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let socks = SocketAddr::from(([127, 0, 0, 1], port));
    let user = random_token(16);
    let pass = random_token(32);

    // Keep the panel's sniffing settings if it had a socks inbound.
    let sniffing = config["inbounds"]
        .as_array()
        .and_then(|a| a.iter().find(|i| i["protocol"] == "socks"))
        .and_then(|i| i.get("sniffing").cloned())
        .unwrap_or_else(|| json!({ "enabled": true, "destOverride": ["http", "tls", "quic"], "routeOnly": false }));
    config["inbounds"] = json!([{
        "tag": "socks",
        "listen": "127.0.0.1",
        "port": port,
        "protocol": "socks",
        "settings": {
            "auth": "password",
            "accounts": [{ "user": user, "pass": pass }],
            "udp": true,
            "ip": "127.0.0.1"
        },
        "sniffing": sniffing
    }]);

    let mut bypass = vec![];
    let Some(outbounds) = config["outbounds"].as_array_mut() else {
        bail!("config has no outbounds");
    };
    for ob in outbounds.iter_mut() {
        let protocol = ob["protocol"].as_str().unwrap_or_default().to_string();
        if protocol == "blackhole" {
            continue;
        }
        let chained = !ob["streamSettings"]["sockopt"]["dialerProxy"].is_null() || !ob["proxySettings"].is_null();
        if !chained {
            if !ob["streamSettings"].is_object() {
                ob["streamSettings"] = json!({});
            }
            if !ob["streamSettings"]["sockopt"].is_object() {
                ob["streamSettings"]["sockopt"] = json!({});
            }
            ob["streamSettings"]["sockopt"]["interface"] = json!(iface);
        }
        if SERVICE_PROTOCOLS.contains(&protocol.as_str()) {
            continue;
        }
        pin_endpoints(ob, &mut resolve, &mut bypass)?;
    }
    bypass.sort();
    bypass.dedup();

    Ok(Runtime { config, socks, user, pass, bypass })
}

/// Replaces hostnames in an outbound's server list with IPs, keeping the name
/// for TLS SNI and HTTP Host.
fn pin_endpoints(
    ob: &mut Value,
    resolve: &mut impl FnMut(&str) -> Result<IpAddr>,
    bypass: &mut Vec<IpAddr>,
) -> Result<()> {
    let mut first_name = None;
    let settings = &mut ob["settings"];
    // vnext (vless/vmess), servers (trojan/ss/socks), or the flat form (hysteria, newer xray).
    let list = ["vnext", "servers"].into_iter().find(|k| settings[*k].is_array());
    let endpoints: Vec<&mut Value> = match list {
        Some(k) => settings[k].as_array_mut().into_iter().flatten().collect(),
        None if settings.get("address").is_some() => vec![settings],
        None => vec![],
    };
    for ep in endpoints {
        let Some(addr) = ep["address"].as_str().map(str::to_string) else { continue };
        let ip = match addr.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => {
                let ip = resolve(&addr)?;
                ep["address"] = json!(ip.to_string());
                first_name.get_or_insert(addr);
                ip
            }
        };
        bypass.push(ip);
    }

    if let Some(name) = first_name {
        let ss = &mut ob["streamSettings"];
        for key in ["tlsSettings", "realitySettings"] {
            if ss[key].is_object() && ss[key]["serverName"].as_str().unwrap_or_default().is_empty() {
                ss[key]["serverName"] = json!(name);
            }
        }
        for key in ["xhttpSettings", "splithttpSettings", "wsSettings", "httpupgradeSettings"] {
            if ss[key].is_object() && ss[key]["host"].as_str().unwrap_or_default().is_empty() {
                ss[key]["host"] = json!(name);
            }
        }
    }
    Ok(())
}

fn random_token(len: usize) -> String {
    std::iter::repeat_with(fastrand::alphanumeric).take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(cfg: Value) -> Server {
        Server::from_xray_config(cfg).unwrap()
    }

    #[test]
    fn rewrites_json_config() {
        let s = server(json!({
            "remarks": "X",
            "inbounds": [{"tag": "socks", "port": 10808, "protocol": "socks",
                          "sniffing": {"enabled": true, "destOverride": ["http","tls"], "routeOnly": false}}],
            "outbounds": [
                {"tag": "proxy", "protocol": "vless",
                 "settings": {"vnext": [{"address": "se.example.com", "port": 443, "users": [{"id": "u"}]}]},
                 "streamSettings": {"network": "xhttp", "security": "tls", "tlsSettings": {}, "xhttpSettings": {"path": "/"}}},
                {"tag": "proxy-2", "protocol": "vless",
                 "settings": {"vnext": [{"address": "9.9.9.9", "port": 443}]},
                 "streamSettings": {"sockopt": {"dialerProxy": "proxy"}}},
                {"tag": "hy", "protocol": "hysteria", "settings": {"address": "8.8.4.4", "port": 8443}},
                {"tag": "direct", "protocol": "freedom"},
                {"tag": "block", "protocol": "blackhole"},
                {"tag": "dns-out", "protocol": "dns"}
            ],
            "routing": {"balancers": [{"tag": "b", "selector": ["proxy"]}]}
        }));
        let rt = build_with(&s, "en0", |h| {
            assert_eq!(h, "se.example.com");
            Ok("1.2.3.4".parse().unwrap())
        })
        .unwrap();
        let c = &rt.config;
        let obs = c["outbounds"].as_array().unwrap();

        assert_eq!(c["inbounds"].as_array().unwrap().len(), 1);
        assert_eq!(c["inbounds"][0]["tag"], "socks");
        assert_eq!(c["inbounds"][0]["settings"]["accounts"][0]["user"], rt.user.as_str());
        assert_eq!(c["inbounds"][0]["sniffing"]["destOverride"], json!(["http", "tls"]));

        assert_eq!(obs[0]["settings"]["vnext"][0]["address"], "1.2.3.4");
        assert_eq!(obs[0]["streamSettings"]["tlsSettings"]["serverName"], "se.example.com");
        assert_eq!(obs[0]["streamSettings"]["xhttpSettings"]["host"], "se.example.com");
        assert_eq!(obs[0]["streamSettings"]["sockopt"]["interface"], "en0");
        assert!(obs[1]["streamSettings"]["sockopt"]["interface"].is_null(), "chained outbound not bound");
        assert_eq!(obs[3]["streamSettings"]["sockopt"]["interface"], "en0", "direct bound");
        assert!(obs[4]["streamSettings"].is_null(), "blackhole untouched");
        assert_eq!(obs[5]["streamSettings"]["sockopt"]["interface"], "en0", "dns bound");
        assert_eq!(c["routing"]["balancers"][0]["tag"], "b", "balancer kept");

        let ips: Vec<String> = rt.bypass.iter().map(|i| i.to_string()).collect();
        assert_eq!(ips, ["1.2.3.4", "8.8.4.4", "9.9.9.9"]);
        assert!(rt.socks.ip().is_loopback());
        assert_eq!(rt.pass.len(), 32);
    }

    #[test]
    fn link_server_gets_full_config() {
        let p = crate::link::parse("vless://id@5.6.7.8:443?security=reality&pbk=K&sni=a.b&type=tcp#L").unwrap();
        let rt = build_with(&Server::from_link(p), "en0", |_| unreachable!()).unwrap();
        assert_eq!(rt.config["outbounds"][0]["protocol"], "vless");
        assert_eq!(rt.config["outbounds"][0]["streamSettings"]["realitySettings"]["publicKey"], "K");
        assert_eq!(rt.bypass, vec!["5.6.7.8".parse::<IpAddr>().unwrap()]);
    }
}

//! Turns a subscription entry into the config xray actually runs.
//!
//! The subscription author's config is kept intact (routing, DNS, balancers,
//! observatory, bridges). Only these things change:
//! 1. `inbounds` become one password-protected SOCKS inbound on loopback,
//!    tagged `socks` like the panel's own, so its routing rules still apply;
//! 2. every outbound that dials the network is bound to the physical
//!    interface, otherwise `direct` traffic would loop back into the TUN;
//! 3. server hostnames are resolved now, before the TUN exists (SNI/Host keep
//!    the name), and those IPs are routed around the TUN;
//! 4. the user's routing (see [`crate::routing`]) is merged into the panel's.
//!
//! In proxy mode [`Runtime::open_proxy`] then moves the inbound to a fixed
//! port without a password: system proxy settings carry no credentials.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs, UdpSocket};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cert_pin;
use crate::routing::{self, GeoData, RoutingSettings};
use crate::server::{Server, Source};
use crate::xray;

pub struct Runtime {
    pub config: Value,
    pub socks: SocketAddr,
    pub user: String,
    pub pass: String,
    pub bypass: Vec<IpAddr>,
    /// SOCKS inbound whose traffic always goes direct (per-app bypass).
    pub direct_socks: Option<SocketAddr>,
    /// Routing entries that were skipped (e.g. a missing geo database).
    pub warnings: Vec<String>,
}

impl Runtime {
    /// Proxy mode: the SOCKS inbound listens on `port` without a password
    /// (xray's SOCKS inbound also speaks HTTP, so one port serves both).
    /// Loopback only, like in TUN mode.
    pub fn open_proxy(&mut self, port: u16) {
        let inbound = &mut self.config["inbounds"][0];
        inbound["port"] = json!(port);
        inbound["settings"]["auth"] = json!("noauth");
        if let Some(settings) = inbound["settings"].as_object_mut() {
            settings.remove("accounts");
        }
        self.socks.set_port(port);
        self.user.clear();
        self.pass.clear();
    }
}

/// User routing to merge into the config.
pub struct Routing<'a> {
    pub settings: &'a RoutingSettings,
    pub geo: &'a GeoData,
}

const SERVICE_PROTOCOLS: &[&str] = &["freedom", "blackhole", "dns", "loopback"];

pub fn build(server: &Server, iface: &str, routing: Option<Routing>) -> Result<Runtime> {
    let mut rt = build_with(server, iface, routing, |host| {
        (host, 0)
            .to_socket_addrs()
            .with_context(|| format!("cannot resolve {host}"))?
            .map(|a| a.ip())
            .find(IpAddr::is_ipv4)
            .with_context(|| format!("{host} has no IPv4 address"))
    })?;
    replace_allow_insecure(&mut rt.config, cert_pin::leaf_sha256);
    Ok(rt)
}

type TlsTarget = (IpAddr, u16, String);

/// Swaps `tlsSettings.allowInsecure`, which xray refuses since 2026-06-01, for
/// a pin of the certificate the server presents now (see [`cert_pin`]). Runs
/// after names are resolved, so it dials the IP xray will. A server that
/// cannot be reached just loses the flag and xray verifies it as usual.
fn replace_allow_insecure(config: &mut Value, leaf_sha256: impl Fn(IpAddr, u16, &str) -> Result<String> + Sync) {
    let Some(outbounds) = config["outbounds"].as_array_mut() else { return };
    // (outbound index, the server to fetch the certificate from)
    let targets: Vec<(usize, Option<TlsTarget>)> = outbounds
        .iter()
        .enumerate()
        .filter(|(_, ob)| ob["streamSettings"]["tlsSettings"]["allowInsecure"] == json!(true))
        .map(|(i, ob)| {
            let tls = &ob["streamSettings"]["tlsSettings"];
            let pinned = tls["pinnedPeerCertSha256"].as_str().is_some_and(|v| !v.trim().is_empty());
            // Hysteria runs over QUIC: nothing answers TLS on TCP there.
            let quic = ob["protocol"] == "hysteria" || ob["streamSettings"]["network"] == "hysteria";
            let target = (!pinned && !quic)
                .then(|| endpoints_of(&ob["settings"]).into_iter().next())
                .flatten()
                .and_then(|ep| {
                    let ip: IpAddr = ep["address"].as_str()?.parse().ok()?;
                    let port = u16::try_from(ep["port"].as_u64()?).ok()?;
                    let sni = tls["serverName"].as_str().filter(|s| !s.is_empty()).map_or_else(|| ip.to_string(), String::from);
                    Some((ip, port, sni))
                });
            (i, target)
        })
        .collect();
    let hashes: Vec<(usize, Option<String>)> = std::thread::scope(|scope| {
        let jobs: Vec<_> = targets
            .iter()
            .map(|(i, t)| (*i, t.as_ref().map(|(ip, port, sni)| scope.spawn(|| leaf_sha256(*ip, *port, sni)))))
            .collect();
        jobs.into_iter().map(|(i, job)| (i, job.and_then(|j| j.join().ok()?.ok()))).collect()
    });
    for (i, hash) in hashes {
        let tls = &mut outbounds[i]["streamSettings"]["tlsSettings"];
        if let Some(tls) = tls.as_object_mut() {
            tls.remove("allowInsecure");
        }
        if let Some(hash) = hash {
            tls["pinnedPeerCertSha256"] = json!(hash);
        }
    }
}

pub fn build_with(
    server: &Server,
    iface: &str,
    routing: Option<Routing>,
    resolve: impl Fn(&str) -> Result<IpAddr> + Sync,
) -> Result<Runtime> {
    let mut config = match &server.source {
        Source::Json(v) => v.clone(),
        Source::Link(p) => xray::config_from_profile(p),
    };
    let mut warnings = match &routing {
        Some(r) => routing::apply(&mut config, r.settings, r.geo),
        None => vec![],
    };
    quiet_log(&mut config);

    let port = free_port()?;
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
    let mut direct_socks = None;
    if routing.as_ref().is_some_and(|r| r.settings.apps.active()) {
        let port = free_port()?;
        let mut inbound = config["inbounds"][0].clone();
        inbound["tag"] = json!(routing::DIRECT_INBOUND);
        inbound["port"] = json!(port);
        config["inbounds"].as_array_mut().unwrap().push(inbound);
        direct_socks = Some(SocketAddr::from(([127, 0, 0, 1], port)));
    }

    let mut bypass = vec![];
    let Some(outbounds) = config["outbounds"].as_array_mut() else {
        bail!("config has no outbounds");
    };
    // All server names at once: a balancer can list dozens, one by one that
    // took seconds before xray could even start.
    let mut names: Vec<String> = outbounds
        .iter()
        .filter(|ob| !SERVICE_PROTOCOLS.contains(&ob["protocol"].as_str().unwrap_or_default()) && ob["protocol"] != "blackhole")
        .flat_map(|ob| endpoints_of(&ob["settings"]))
        .filter_map(|ep| ep["address"].as_str())
        .filter(|a| a.parse::<IpAddr>().is_err())
        .map(String::from)
        .collect();
    names.sort();
    names.dedup();
    let results: Vec<(String, Result<IpAddr>)> = std::thread::scope(|scope| {
        let jobs: Vec<_> = names.iter().map(|n| (n, scope.spawn(|| resolve(n)))).collect();
        jobs.into_iter()
            .map(|(n, job)| (n.clone(), job.join().unwrap_or_else(|_| Err(anyhow::anyhow!("resolving {n} panicked")))))
            .collect()
    });
    let mut resolved: HashMap<String, IpAddr> = HashMap::new();
    let mut failed: HashMap<String, anyhow::Error> = HashMap::new();
    for (name, result) in results {
        match result {
            Ok(ip) => drop(resolved.insert(name, ip)),
            Err(e) => drop(failed.insert(name, e)),
        }
    }
    if !failed.is_empty() {
        drop_unresolved_nodes(&mut config, &failed, &mut warnings);
    }
    let Some(outbounds) = config["outbounds"].as_array_mut() else {
        bail!("config has no outbounds");
    };
    // A name still unresolved is not a spare balancer node: connecting fails.
    let mut resolve = |host: &str| match resolved.get(host) {
        Some(ip) => Ok(*ip),
        None => Err(failed.remove(host).unwrap_or_else(|| anyhow::anyhow!("cannot resolve {host}"))),
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

    Ok(Runtime { config, socks, user, pass, bypass, direct_socks, warnings })
}

/// Removes balancer nodes whose server name did not resolve (a dead domain
/// among dozens took the whole connection down), as long as each balancer
/// keeps a node. Other outbounds are left: their failure is reported.
fn drop_unresolved_nodes(config: &mut Value, failed: &HashMap<String, anyhow::Error>, warnings: &mut Vec<String>) {
    let selectors: Vec<Vec<String>> = config["routing"]["balancers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| b["selector"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect())
        .collect();
    let Some(outbounds) = config["outbounds"].as_array() else { return };
    let tag = |o: &Value| o["tag"].as_str().unwrap_or_default().to_string();
    let in_balancer = |t: &str, sel: &[String]| sel.iter().any(|s| t.starts_with(s.as_str()));
    let dead = |o: &Value| endpoints_of(&o["settings"]).iter().any(|ep| ep["address"].as_str().is_some_and(|a| failed.contains_key(a)));
    let mut drop_tags: Vec<String> = outbounds
        .iter()
        .filter(|o| dead(o) && selectors.iter().any(|sel| in_balancer(&tag(o), sel)))
        .map(tag)
        .collect();
    // Keep a balancer whole rather than empty it: its error is the real one.
    for sel in &selectors {
        let members: Vec<String> = outbounds.iter().map(tag).filter(|t| in_balancer(t, sel)).collect();
        if !members.is_empty() && members.iter().all(|m| drop_tags.contains(m)) {
            drop_tags.retain(|t| !members.contains(t));
        }
    }
    if drop_tags.is_empty() {
        return;
    }
    let hosts: Vec<&str> = outbounds
        .iter()
        .filter(|o| drop_tags.contains(&tag(o)))
        .flat_map(|o| endpoints_of(&o["settings"]))
        .filter_map(|ep| ep["address"].as_str())
        .filter(|a| failed.contains_key(*a))
        .collect();
    let mut hosts: Vec<String> = hosts.into_iter().map(String::from).collect();
    hosts.sort();
    hosts.dedup();
    warnings.push(format!("Пропущено узлов балансера: {} — не найден адрес {}", drop_tags.len(), hosts.join(", ")));
    if let Some(o) = config["outbounds"].as_array_mut() {
        o.retain(|o| !drop_tags.contains(&tag(o)));
    }
}

/// No access log and nothing chattier than warnings: panels often turn them
/// on, and a line per connection buried xray's own error (why it stopped)
/// under thousands of "accepted" lines in the log tail of reports.
fn quiet_log(config: &mut Value) {
    if !config["log"].is_object() {
        config["log"] = json!({});
    }
    config["log"]["access"] = json!("none");
    let level = config["log"]["loglevel"].as_str().unwrap_or("warning");
    if matches!(level, "debug" | "info") {
        config["log"]["loglevel"] = json!("warning");
    }
}

/// An outbound's servers: vnext (vless/vmess), servers (trojan/ss/socks), or
/// the flat form (hysteria, newer xray).
fn endpoints_of(settings: &Value) -> Vec<&Value> {
    match ["vnext", "servers"].into_iter().find(|k| settings[*k].is_array()) {
        Some(k) => settings[k].as_array().into_iter().flatten().collect(),
        None if settings.get("address").is_some() => vec![settings],
        None => vec![],
    }
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

/// A loopback port free for both TCP and UDP: the SOCKS inbound listens on
/// both. Windows reserves port ranges (Hyper-V, WinNAT) per protocol, so a
/// port the system hands out for TCP can still be forbidden for UDP.
pub fn free_port() -> Result<u16> {
    for _ in 0..50 {
        let tcp = TcpListener::bind("127.0.0.1:0")?;
        let port = tcp.local_addr()?.port();
        if UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("нет свободного локального порта")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_port_is_free_for_tcp_and_udp() {
        let port = free_port().unwrap();
        let _tcp = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let _udp = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    }

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
            "routing": {"balancers": [{"tag": "b", "selector": ["proxy"]}]},
            "log": {"access": "", "loglevel": "debug"}
        }));
        let rt = build_with(&s, "en0", None, |h| {
            assert_eq!(h, "se.example.com");
            Ok("1.2.3.4".parse().unwrap())
        })
        .unwrap();
        let c = &rt.config;
        assert_eq!(c["log"], json!({"access": "none", "loglevel": "warning"}), "quiet log");
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
    fn allow_insecure_becomes_a_pinned_certificate() {
        let tls = |extra: Value| {
            let mut t = json!({"serverName": "a.example", "allowInsecure": true});
            t.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            t
        };
        let mut config = json!({"outbounds": [
            {"protocol": "trojan", "settings": {"servers": [{"address": "1.2.3.4", "port": 8443}]},
             "streamSettings": {"network": "ws", "security": "tls", "tlsSettings": tls(json!({}))}},
            {"protocol": "vless", "settings": {"vnext": [{"address": "5.6.7.8", "port": 443}]},
             "streamSettings": {"security": "tls", "tlsSettings": tls(json!({"pinnedPeerCertSha256": "ab"}))}},
            {"protocol": "hysteria", "settings": {"address": "9.9.9.9", "port": 443},
             "streamSettings": {"network": "hysteria", "security": "tls", "tlsSettings": tls(json!({}))}},
            {"protocol": "vless", "settings": {"vnext": [{"address": "4.4.4.4", "port": 443}]},
             "streamSettings": {"security": "tls", "tlsSettings": tls(json!({}))}},
            {"protocol": "vless", "settings": {"vnext": [{"address": "8.8.8.8", "port": 443}]},
             "streamSettings": {"security": "tls", "tlsSettings": {"serverName": "b.example"}}}
        ]});
        replace_allow_insecure(&mut config, |ip, port, sni| match (ip.to_string().as_str(), port, sni) {
            ("1.2.3.4", 8443, "a.example") => Ok("11".repeat(32)),
            ("4.4.4.4", ..) => bail!("unreachable"),
            other => panic!("unexpected dial {other:?}"),
        });
        let tls_of = |i: usize| config["outbounds"][i]["streamSettings"]["tlsSettings"].clone();
        assert_eq!(tls_of(0), json!({"serverName": "a.example", "pinnedPeerCertSha256": "11".repeat(32)}));
        assert_eq!(tls_of(1), json!({"serverName": "a.example", "pinnedPeerCertSha256": "ab"}), "own pin kept");
        assert_eq!(tls_of(2), json!({"serverName": "a.example"}), "QUIC: flag dropped, nothing dialed");
        assert_eq!(tls_of(3), json!({"serverName": "a.example"}), "unreachable: flag dropped");
        assert_eq!(tls_of(4), json!({"serverName": "b.example"}), "no flag: untouched");
    }

    #[test]
    fn link_server_gets_full_config() {
        let p = crate::link::parse("vless://id@5.6.7.8:443?security=reality&pbk=K&sni=a.b&type=tcp#L").unwrap();
        let rt = build_with(&Server::from_link(p), "en0", None, |_| unreachable!()).unwrap();
        assert_eq!(rt.config["outbounds"][0]["protocol"], "vless");
        assert_eq!(rt.config["outbounds"][0]["streamSettings"]["realitySettings"]["publicKey"], "K");
        assert_eq!(rt.bypass, vec!["5.6.7.8".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn unresolvable_balancer_nodes_are_skipped() {
        let node = |tag: &str, host: &str| json!({"tag": tag, "protocol": "vless",
            "settings": {"vnext": [{"address": host, "port": 443, "users": [{"id": "u"}]}]}});
        let cfg = |hosts: &[&str]| json!({
            "outbounds": hosts.iter().enumerate().map(|(i, h)| node(&format!("proxy-{i}"), h)).collect::<Vec<_>>(),
            "routing": {"balancers": [{"tag": "b", "selector": ["proxy"]}], "rules": [{"network": "tcp,udp", "balancerTag": "b"}]}
        });
        let resolve = |h: &str| if h.starts_with("dead") { bail!("cannot resolve {h}") } else { Ok("1.2.3.4".parse().unwrap()) };
        let rt = build_with(&server(cfg(&["ok.example", "dead.example", "ok2.example"])), "en0", None, resolve).unwrap();
        let tags: Vec<_> = rt.config["outbounds"].as_array().unwrap().iter().map(|o| o["tag"].clone()).collect();
        assert_eq!(tags, [json!("proxy-0"), json!("proxy-2")]);
        assert!(rt.warnings.iter().any(|w| w.contains("dead.example")));
        let err = build_with(&server(cfg(&["dead.example", "dead2.example"])), "en0", None, resolve).err().unwrap();
        assert!(format!("{err:#}").contains("cannot resolve"), "all nodes dead: the real error");
    }

    #[test]
    fn resolves_server_names_in_parallel() {
        let outbounds: Vec<Value> = (0..10)
            .map(|i| json!({"tag": format!("p{i}"), "protocol": "vless",
                            "settings": {"vnext": [{"address": format!("s{i}.example.com"), "port": 443, "users": [{"id": "u"}]}]}}))
            .collect();
        let s = server(json!({ "outbounds": outbounds }));
        let started = std::time::Instant::now();
        let rt = build_with(&s, "eth0", None, |h| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok(format!("10.0.0.{}", &h[1..h.find('.').unwrap()]).parse().unwrap())
        })
        .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "10 × 300 ms one by one would be 3 s");
        assert_eq!(rt.config["outbounds"][7]["settings"]["vnext"][0]["address"], "10.0.0.7");
        assert_eq!(rt.bypass.len(), 10);
    }

    #[test]
    fn proxy_mode_opens_the_inbound_on_its_port() {
        let s = server(json!({
            "outbounds": [{"tag": "proxy", "protocol": "vless",
                           "settings": {"vnext": [{"address": "1.2.3.4", "port": 443, "users": [{"id": "u"}]}]}}]
        }));
        let mut rt = build_with(&s, "eth0", None, |_| unreachable!()).unwrap();
        rt.open_proxy(10808);
        let inbound = &rt.config["inbounds"][0];
        assert_eq!(inbound["port"], 10808);
        assert_eq!(inbound["listen"], "127.0.0.1");
        assert_eq!(inbound["settings"]["auth"], "noauth");
        assert!(inbound["settings"].get("accounts").is_none());
        assert_eq!(rt.socks, "127.0.0.1:10808".parse().unwrap());
        assert!(rt.user.is_empty() && rt.pass.is_empty());
    }

    #[test]
    fn routing_binds_direct_and_adds_bypass_inbound() {
        let p = crate::link::parse("vless://id@5.6.7.8:443?security=reality&pbk=K&sni=a.b&type=tcp#L").unwrap();
        let settings = RoutingSettings {
            apps: crate::routing::AppRouting { mode: crate::routing::AppMode::Bypass, apps: vec!["curl".into()] },
            ..Default::default()
        };
        let geo = GeoData::load(None, None);
        let rt = build_with(&Server::from_link(p), "en0", Some(Routing { settings: &settings, geo: &geo }), |_| {
            unreachable!()
        })
        .unwrap();
        let c = &rt.config;
        let direct = c["outbounds"].as_array().unwrap().iter().find(|o| o["tag"] == routing::DIRECT_TAG).unwrap();
        assert_eq!(direct["streamSettings"]["sockopt"]["interface"], "en0");
        assert_eq!(c["inbounds"][1]["tag"], routing::DIRECT_INBOUND);
        assert_eq!(c["inbounds"][1]["settings"]["accounts"][0]["pass"], rt.pass.as_str());
        assert_eq!(rt.direct_socks.unwrap().port(), c["inbounds"][1]["port"].as_u64().unwrap() as u16);
        assert_eq!(rt.warnings, ["нет базы для geoip:private"], "no geo files in tests");
    }
}

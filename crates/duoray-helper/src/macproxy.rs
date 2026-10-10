//! macOS system proxy for proxy mode, set with `networksetup` (changing
//! network settings needs root, hence here and not in the GUI). The previous
//! settings of every network service are written to [`STATE`] before anything
//! changes, so a helper that died meanwhile restores them on its next start.

#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(target_os = "macos")]
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use tracing::{info, warn};

#[cfg(target_os = "macos")]
pub const STATE: &str = "/var/run/duoray-proxy.json";
/// HTTP, HTTPS and SOCKS: xray's inbound answers all three on one port.
#[cfg(target_os = "macos")]
const KINDS: [&str; 3] = ["webproxy", "securewebproxy", "socksfirewallproxy"];
#[cfg(target_os = "macos")]
const BYPASS: &[&str] =
    &["localhost", "127.0.0.1", "::1", "*.local", "169.254/16", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxyState {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    /// In `KINDS` order.
    pub proxies: Vec<ProxyState>,
    pub auto_proxy: bool,
    pub bypass: Vec<String>,
}

/// Points every enabled network service at `127.0.0.1:port`.
#[cfg(target_os = "macos")]
pub fn enable(port: u16) -> Result<()> {
    restore_saved();
    let saved = parse_services(&networksetup(&["-listallnetworkservices"])?)
        .into_iter()
        .map(|name| snapshot(&name))
        .collect::<Result<Vec<_>>>()?;
    if saved.is_empty() {
        bail!("no network services");
    }
    std::fs::write(STATE, serde_json::to_vec(&saved)?).context("saving the proxy settings")?;
    let apply = || -> Result<()> {
        let port = port.to_string();
        for s in &saved {
            let n = s.name.as_str();
            for kind in KINDS {
                networksetup(&[&format!("-set{kind}"), n, "127.0.0.1", &port])?;
                networksetup(&[&format!("-set{kind}state"), n, "on"])?;
            }
            if s.auto_proxy {
                networksetup(&["-setautoproxystate", n, "off"])?;
            }
            let mut args = vec!["-setproxybypassdomains", n];
            args.extend(BYPASS);
            networksetup(&args)?;
        }
        Ok(())
    };
    if let Err(e) = apply() {
        restore_saved();
        return Err(e);
    }
    info!("system proxy 127.0.0.1:{port} on {} service(s)", saved.len());
    Ok(())
}

/// Puts back what [`enable`] saved, if anything (also after a crash).
#[cfg(target_os = "macos")]
pub fn restore_saved() {
    let Ok(data) = std::fs::read(STATE) else { return };
    match serde_json::from_slice::<Vec<Service>>(&data) {
        Ok(saved) => {
            for s in &saved {
                restore(s);
            }
            info!("system proxy restored");
        }
        Err(e) => warn!("unreadable {STATE}: {e}"),
    }
    let _ = std::fs::remove_file(STATE);
}

#[cfg(target_os = "macos")]
fn snapshot(name: &str) -> Result<Service> {
    let mut proxies = vec![];
    for kind in KINDS {
        proxies.push(parse_proxy(&networksetup(&[&format!("-get{kind}"), name])?));
    }
    let auto_proxy = parse_proxy(&networksetup(&["-getautoproxyurl", name])?).enabled;
    let bypass = parse_bypass(&networksetup(&["-getproxybypassdomains", name])?);
    Ok(Service { name: name.to_string(), proxies, auto_proxy, bypass })
}

#[cfg(target_os = "macos")]
fn restore(s: &Service) {
    let n = s.name.as_str();
    let run = |args: &[&str]| {
        if let Err(e) = networksetup(args) {
            warn!("restoring the proxy of {n}: {e:#}");
        }
    };
    for (kind, p) in KINDS.iter().zip(&s.proxies) {
        if !p.server.is_empty() {
            run(&[&format!("-set{kind}"), n, &p.server, &p.port.to_string()]);
        }
        run(&[&format!("-set{kind}state"), n, if p.enabled { "on" } else { "off" }]);
    }
    if s.auto_proxy {
        run(&["-setautoproxystate", n, "on"]);
    }
    let mut args = vec!["-setproxybypassdomains", n];
    if s.bypass.is_empty() {
        args.push("Empty");
    } else {
        args.extend(s.bypass.iter().map(String::as_str));
    }
    run(&args);
}

#[cfg(target_os = "macos")]
fn networksetup(args: &[&str]) -> Result<String> {
    let out = Command::new("/usr/sbin/networksetup").args(args).output().context("running networksetup")?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // It reports some failures on stdout with exit code 0.
    if !out.status.success() || text.contains("** Error") {
        bail!("networksetup {}: {}{}", args.join(" "), text.trim(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(text)
}

/// `-listallnetworkservices`: a header line, then names; `*` marks disabled ones.
fn parse_services(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.contains("asterisk") && !l.starts_with('*'))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// `-getwebproxy` and friends ("Enabled: Yes", "Server: …", "Port: …"); also
/// `-getautoproxyurl` ("URL: …", "Enabled: No").
fn parse_proxy(text: &str) -> ProxyState {
    let mut p = ProxyState::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key.trim() {
            "Enabled" => p.enabled = value.eq_ignore_ascii_case("yes"),
            "Server" => p.server = value.to_string(),
            "Port" => p.port = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    p
}

/// `-getproxybypassdomains`: one domain per line, or a sentence when empty.
fn parse_bypass(text: &str) -> Vec<String> {
    if text.starts_with("There aren't") {
        return vec![];
    }
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_networksetup_output() {
        let list = "An asterisk (*) denotes that a network service is disabled.\nWi-Fi\n*Thunderbolt Bridge\nUSB 10/100/1000 LAN\n";
        assert_eq!(parse_services(list), ["Wi-Fi", "USB 10/100/1000 LAN"]);

        let on = parse_proxy("Enabled: Yes\nServer: 10.0.0.1\nPort: 3128\nAuthenticated Proxy Enabled: 0\n");
        assert_eq!(on, ProxyState { enabled: true, server: "10.0.0.1".into(), port: 3128 });
        let off = parse_proxy("Enabled: No\nServer: \nPort: 0\nAuthenticated Proxy Enabled: 0\n");
        assert_eq!(off, ProxyState::default());
        assert!(!parse_proxy("URL: (null)\nEnabled: No\n").enabled);

        assert!(parse_bypass("There aren't any bypass domains set on Wi-Fi.\n").is_empty());
        assert_eq!(parse_bypass("*.local\n169.254/16\n"), ["*.local", "169.254/16"]);
    }
}

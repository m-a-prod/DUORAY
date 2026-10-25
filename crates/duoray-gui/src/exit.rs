//! Where a live connection comes out: the node a balancer picked (asked from
//! xray's API) and the exit country (Cloudflare's trace through the tunnel).

use std::net::SocketAddr;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use duoray_core::balancer::{self, Trace};

use crate::connection;

const TRACE_URL: &str = "https://1.1.1.1/cdn-cgi/trace";

/// The outbound tag the balancer `tag` uses now; `None` when it has picked
/// none (no node answered yet: traffic goes to the fallback).
pub fn selected_node(api: SocketAddr, tag: &str) -> Result<Option<String>> {
    let (xray, _) = connection::find_xray()?;
    let mut cmd = Command::new(xray);
    cmd.args(["api", "bi", &format!("--server={api}"), "-t", "3", tag]);
    let out = connection::hidden(&mut cmd).output().context("xray api bi")?;
    if !out.status.success() {
        bail!("xray api bi: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(balancer::parse_selected(&String::from_utf8_lossy(&out.stdout)))
}

/// Cloudflare's view of the connection through xray's SOCKS inbound.
pub fn trace(socks: SocketAddr, user: &str, pass: &str) -> Result<Trace> {
    // Proxy mode's inbound has no password.
    let proxy = ureq::Proxy::new(&if user.is_empty() { format!("socks5://{socks}") } else { format!("socks5://{user}:{pass}@{socks}") })?;
    let agent: ureq::Agent = ureq::Agent::config_builder().proxy(Some(proxy)).timeout_global(Some(Duration::from_secs(8))).build().into();
    let body = agent.get(TRACE_URL).call()?.body_mut().read_to_string()?;
    balancer::parse_trace(&body).context("no loc in the trace")
}

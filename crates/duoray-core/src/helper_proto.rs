//! Protocol between the unprivileged GUI and the privileged helper service
//! (newline-delimited JSON over a local socket).
//!
//! A TUN session lives exactly as long as the client connection that started
//! it: if the GUI exits or crashes, the helper tears the TUN down and restores
//! routes/DNS.
//!
//! Strictly request → response: the helper never writes on its own. (On
//! Windows, synchronous reads and writes on one pipe block each other, so the
//! client cannot keep a reader waiting.) The client polls `Status` instead.

use std::io::{self, BufRead, Write};
use std::net::{IpAddr, SocketAddr};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped on protocol changes; the GUI offers to reinstall the helper.
/// v3: per-app routing (`TunRequest::apps`).
pub const PROTOCOL: u32 = 3;
/// Oldest helper the GUI still works with (without the newer features).
pub const MIN_PROTOCOL: u32 = 2;
/// First protocol with per-app routing.
pub const PROTOCOL_APPS: u32 = 3;

#[cfg(unix)]
pub const SOCKET_PATH: &str = "/var/run/duoray-helper.sock";

#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\duoray-helper";

/// Windows service name of the helper.
pub const SERVICE_NAME: &str = "DuorayHelper";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Hello,
    Start(TunRequest),
    Stop,
    /// Is the TUN of this connection still up? Polled by the client.
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunRequest {
    /// xray SOCKS inbound; must be a loopback address.
    pub socks: SocketAddr,
    pub user: String,
    pub pass: String,
    /// Proxy server addresses that must bypass the TUN.
    pub bypass: Vec<IpAddr>,
    pub ipv6: bool,
    /// Per-app routing (protocol 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<AppsRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppsRequest {
    /// `true`: only the listed apps use the proxy; `false`: they bypass it.
    pub only: bool,
    pub apps: Vec<String>,
    /// xray SOCKS inbound routed direct; loopback, same credentials.
    pub direct_socks: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello { version: String, protocol: u32 },
    Started { tun: String },
    Stopped,
    /// `error` is set once when the TUN died on its own.
    Status { running: bool, error: Option<String> },
    Error { message: String },
}

pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// `Ok(None)` on a clean EOF.
pub fn read_msg<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let req = Request::Start(TunRequest {
            socks: "127.0.0.1:1080".parse().unwrap(),
            user: "u".into(),
            pass: "p".into(),
            bypass: vec!["1.2.3.4".parse().unwrap()],
            ipv6: true,
            apps: None,
        });
        let mut buf = vec![];
        write_msg(&mut buf, &req).unwrap();
        let back: Request = read_msg(&mut buf.as_slice()).unwrap().unwrap();
        assert!(matches!(back, Request::Start(t) if t.user == "u"));
        assert!(read_msg::<_, Request>(&mut &b""[..]).unwrap().is_none());
    }

    /// A v2 client's request (no `apps`) still parses in a v3 helper.
    #[test]
    fn v2_request_parses() {
        let line = br#"{"cmd":"start","socks":"127.0.0.1:1","user":"u","pass":"p","bypass":[],"ipv6":true}
"#;
        let back: Request = read_msg(&mut &line[..]).unwrap().unwrap();
        assert!(matches!(back, Request::Start(t) if t.apps.is_none()));
    }
}

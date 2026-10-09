//! Subscription download and decoding.
//!
//! The JSON form (full xray configs) is preferred, because only it carries
//! what the subscription author intended: routing, DNS, balancers, chained
//! bridges. Share links are the fallback.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_NO_PAD_INDIFFERENT, URL_SAFE_NO_PAD_INDIFFERENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::link;
use crate::server::Server;

/// Metadata from the de-facto standard subscription headers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubInfo {
    pub title: Option<String>,
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub total: Option<u64>,
    /// Unix seconds.
    pub expire: Option<u64>,
    pub update_interval_hours: Option<u32>,
    pub support_url: Option<String>,
    pub web_page_url: Option<String>,
    pub announce: Option<String>,
    /// Backup subscription URL, tried when the main one is unreachable.
    pub fallback_url: Option<String>,
    /// Happ directives: the panel moved; replace the stored URL (or just its host).
    pub new_url: Option<String>,
    pub new_domain: Option<String>,
}

#[derive(Debug)]
pub struct Fetched {
    pub info: SubInfo,
    pub servers: Vec<Server>,
    /// Entries that could not be used, with the reason.
    pub skipped: Vec<String>,
    /// True if the servers came from the JSON form.
    pub json: bool,
}

/// Fetches a subscription; if `url` is unreachable and the panel earlier
/// advertised a `fallback-url`, that one is tried instead.
pub fn fetch(url: &str, fallback: Option<&str>, headers: &[(String, String)]) -> Result<Fetched> {
    fetch_with_proxy(url, fallback, headers, None)
}

/// An explicit SOCKS5 proxy resolves subscription hostnames through xray,
/// avoiding a broken system resolver while the VPN is active.
pub fn fetch_with_proxy(url: &str, fallback: Option<&str>, headers: &[(String, String)], proxy: Option<&str>) -> Result<Fetched> {
    let mut config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(25)));
    if let Some(proxy) = proxy {
        config = config.proxy(Some(ureq::Proxy::new(proxy)?));
    }
    let agent: ureq::Agent = config.build().into();
    match fetch_one(&agent, url, headers) {
        Ok(f) => Ok(f),
        Err(e) => match fallback.filter(|f| !f.trim().eq_ignore_ascii_case(url.trim())) {
            // The primary failure is the actionable one if both fail.
            Some(fb) => fetch_one(&agent, fb, headers).map_err(|_| e),
            None => Err(e),
        },
    }
}

fn fetch_one(agent: &ureq::Agent, url: &str, headers: &[(String, String)]) -> Result<Fetched> {
    // 1. The URL as given. Panels choose the format per client here, and only
    //    here do Remnawave's response rules (e.g. additionalExtendedClientsRegex,
    //    which adds meta.serverDescription) apply; `/json` ignores them.
    let base = get(agent, url, headers);
    if let Ok(f) = &base
        && f.json
    {
        return base;
    }

    // 2. Share links (or an error): full JSON configs beat links, try `/json`.
    if let Some(json_url) = json_endpoint(url)
        && let Ok(f) = get(agent, &json_url, headers)
        && f.json
        && !f.servers.is_empty()
    {
        // Keep the richer metadata headers of the base response if we had one.
        return Ok(match base {
            Ok(b) => Fetched { info: merge_info(b.info, f.info), ..f },
            Err(_) => f,
        });
    }

    // 3. Whatever the base URL gave.
    base
}

fn merge_info(primary: SubInfo, fallback: SubInfo) -> SubInfo {
    SubInfo {
        title: primary.title.or(fallback.title),
        upload: primary.upload.or(fallback.upload),
        download: primary.download.or(fallback.download),
        total: primary.total.or(fallback.total),
        expire: primary.expire.or(fallback.expire),
        update_interval_hours: primary.update_interval_hours.or(fallback.update_interval_hours),
        support_url: primary.support_url.or(fallback.support_url),
        web_page_url: primary.web_page_url.or(fallback.web_page_url),
        announce: primary.announce.or(fallback.announce),
        fallback_url: primary.fallback_url.or(fallback.fallback_url),
        new_url: primary.new_url.or(fallback.new_url),
        new_domain: primary.new_domain.or(fallback.new_domain),
    }
}

/// Remnawave-style panels serve xray JSON at `<subscription>/json`.
pub fn json_endpoint(url: &str) -> Option<String> {
    let mut u = Url::parse(url).ok()?;
    if u.path_segments()?.rfind(|s| !s.is_empty()) == Some("json") {
        return None;
    }
    u.path_segments_mut().ok()?.pop_if_empty().push("json");
    Some(u.to_string())
}

fn get(agent: &ureq::Agent, url: &str, headers: &[(String, String)]) -> Result<Fetched> {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let mut resp = req.call().with_context(|| format!("GET {url}"))?;

    let body = resp
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_to_string()
        .context("reading subscription body")?;
    // Panels that cannot set headers put the same fields in `#key: value` comment lines.
    let comments = body_comments(&body);
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .or_else(|| comments.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()))
            // Some panels render missing values literally as "null".
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
    };
    let mut info = header("subscription-userinfo").map(|v| parse_userinfo(&v)).unwrap_or_default();
    info.title = header("profile-title").map(|t| decode_text(&t));
    info.update_interval_hours = header("profile-update-interval").and_then(|v| v.parse().ok());
    info.support_url = header("support-url");
    info.web_page_url = header("profile-web-page-url");
    info.announce = header("announce").map(|t| decode_text(&t));
    info.fallback_url = header("fallback-url");
    info.new_url = header("new-url");
    info.new_domain = header("new-domain");
    let (servers, skipped, json) = parse_body(&body)?;
    if servers.is_empty() {
        match skipped.first() {
            Some(why) => bail!("no usable servers ({} skipped, e.g. {why})", skipped.len()),
            None => bail!("subscription is empty"),
        }
    }
    Ok(Fetched { info, servers, skipped, json })
}

/// Accepts an xray JSON array/object, share links, or either base64-encoded.
pub fn parse_body(body: &str) -> Result<(Vec<Server>, Vec<String>, bool)> {
    let body = body.trim_start_matches('\u{feff}').trim();
    if body.starts_with('<') {
        bail!("got an HTML page instead of a subscription; check the URL");
    }
    if body.starts_with('[') || body.starts_with('{') {
        let (s, k) = parse_json(body)?;
        return Ok((s, k, true));
    }
    let text = if body.contains("://") {
        body.to_string()
    } else {
        let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = STANDARD_NO_PAD_INDIFFERENT
            .decode(&compact)
            .or_else(|_| URL_SAFE_NO_PAD_INDIFFERENT.decode(&compact))
            .context("body is neither JSON, share links nor base64")?;
        let text = String::from_utf8(bytes).context("decoded body is not UTF-8")?;
        let t = text.trim_start();
        if t.starts_with('[') || t.starts_with('{') {
            let (s, k) = parse_json(t)?;
            return Ok((s, k, true));
        }
        text
    };

    let mut servers = vec![];
    let mut skipped = vec![];
    for line in text.lines().map(str::trim).filter(|l| l.contains("://")) {
        match link::parse(line) {
            Ok(p) => servers.push(Server::from_link(p)),
            Err(e) => skipped.push(format!("{e:#}")),
        }
    }
    Ok((servers, skipped, false))
}

fn parse_json(body: &str) -> Result<(Vec<Server>, Vec<String>)> {
    let root: Value = serde_json::from_str(body).context("invalid JSON")?;
    let items = match root {
        Value::Array(a) => a,
        obj @ Value::Object(_) => vec![obj],
        _ => bail!("unexpected JSON"),
    };
    let mut servers = vec![];
    let mut skipped = vec![];
    for (i, item) in items.into_iter().enumerate() {
        match Server::from_xray_config(item) {
            Some(s) => servers.push(s),
            None => skipped.push(format!("config #{} has no proxy outbound", i + 1)),
        }
    }
    Ok((servers, skipped))
}

/// `#profile-title: Name` style lines, from the body or its base64 decoding.
fn body_comments(body: &str) -> Vec<(String, String)> {
    let body = body.trim_start_matches('\u{feff}').trim();
    if body.starts_with('[') || body.starts_with('{') {
        return vec![];
    }
    let text = if body.contains("://") || body.starts_with('#') {
        body.to_string()
    } else {
        let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        match STANDARD_NO_PAD_INDIFFERENT.decode(&compact).ok().and_then(|b| String::from_utf8(b).ok()) {
            Some(t) => t,
            None => return vec![],
        }
    };
    text.lines()
        .filter_map(|l| l.trim().strip_prefix('#'))
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, v)| !k.is_empty() && !k.contains(' ') && !v.is_empty())
        .collect()
}

/// `upload=1; download=2; total=3; expire=4`
fn parse_userinfo(v: &str) -> SubInfo {
    let mut info = SubInfo::default();
    for part in v.split(';') {
        let Some((k, val)) = part.split_once('=') else { continue };
        // Some panels send floats ("1.5e10") or empty values; 0 means "unlimited".
        let num = val.trim().parse::<f64>().ok().filter(|n| *n > 0.0).map(|n| n as u64);
        match k.trim() {
            "upload" => info.upload = num,
            "download" => info.download = num,
            "total" => info.total = num,
            "expire" => info.expire = num,
            _ => {}
        }
    }
    info
}

/// Text headers are either plain or `base64:<data>` (prefix in any case).
fn decode_text(v: &str) -> String {
    let Some(b) = v.get(..7).filter(|p| p.eq_ignore_ascii_case("base64:")).map(|_| &v[7..]) else {
        return v.to_string();
    };
    let compact: String = b.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD_NO_PAD_INDIFFERENT
        .decode(&compact)
        .or_else(|_| URL_SAFE_NO_PAD_INDIFFERENT.decode(&compact))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| b.trim().to_string())
}

/// Resolves Happ's `new-url` / `new-domain`: the panel has moved (e.g. its
/// domain got blocked) and the stored URL should be replaced for good. Only
/// HTTPS targets are accepted; `new-url` wins over `new-domain`.
pub fn replacement_url(info: &SubInfo, current: &str) -> Option<String> {
    if let Some(new) = &info.new_url {
        let u = Url::parse(new.trim()).ok().filter(|u| u.scheme() == "https")?;
        return Some(u.to_string()).filter(|n| !n.eq_ignore_ascii_case(current.trim()));
    }
    if let Some(domain) = &info.new_domain {
        let mut u = Url::parse(current.trim()).ok()?;
        u.set_host(Some(domain.trim())).ok()?;
        if u.scheme() != "https" {
            return None;
        }
        return Some(u.to_string()).filter(|n| !n.eq_ignore_ascii_case(current.trim()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    const LINKS: &str = "vless://00000000-1111-2222-3333-444444444444@1.2.3.4:443?type=tcp&security=reality&pbk=K&sni=a.b#One\n\
        trojan://pw@t.example:443#Two\n\
        hysteria2://pw@h.example:443#Three\n";

    #[test]
    fn base64_links() {
        let (s, k, json) = parse_body(&STANDARD.encode(LINKS)).unwrap();
        assert_eq!((s.len(), k.len(), json), (2, 1, false));
        assert_eq!(s[0].name, "One");
    }

    #[test]
    fn wrapped_base64() {
        let wrapped: String = STANDARD
            .encode(LINKS)
            .as_bytes()
            .chunks(20)
            .map(|c| std::str::from_utf8(c).unwrap().to_string() + "\r\n")
            .collect();
        assert_eq!(parse_body(&wrapped).unwrap().0.len(), 2);
    }

    #[test]
    fn json_array() {
        let body = r#"[{"remarks":"A","outbounds":[{"tag":"proxy","protocol":"vless","settings":{"vnext":[{"address":"1.1.1.1","port":443}]}}]},
                      {"remarks":"B","outbounds":[{"protocol":"freedom"}]}]"#;
        let (s, k, json) = parse_body(body).unwrap();
        assert_eq!((s.len(), k.len(), json), (1, 1, true));
    }

    #[test]
    fn proxy_resolves_subscription_hostname() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = format!("socks5h://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut hello = [0; 2];
            stream.read_exact(&mut hello).unwrap();
            assert_eq!(hello[0], 5);
            let mut methods = vec![0; hello[1] as usize];
            stream.read_exact(&mut methods).unwrap();
            stream.write_all(&[5, 0]).unwrap();
            let mut request = [0; 5];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request[..4], &[5, 1, 0, 3], "SOCKS must receive a domain, not a locally resolved IP");
            let mut host = vec![0; request[4] as usize];
            stream.read_exact(&mut host).unwrap();
            assert_eq!(host, b"subscription.invalid");
            let mut port = [0; 2];
            stream.read_exact(&mut port).unwrap();
            stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80]).unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
                assert!(header.len() < 16384);
            }
            let body = r#"[{"remarks":"Test","outbounds":[{"protocol":"vless","settings":{"vnext":[{"address":"198.51.100.1","port":443}]}}]}]"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let fetched = fetch_with_proxy("http://subscription.invalid/sub", None, &[], Some(&proxy)).unwrap();
        assert_eq!(fetched.servers.len(), 1);
        worker.join().unwrap();
    }

    #[test]
    fn comment_headers() {
        let body = format!("#profile-title: base64:{}\n#subscription-userinfo: upload=1; download=2; total=3\n{LINKS}", STANDARD.encode("Моя"));
        let c = body_comments(&STANDARD.encode(&body));
        assert!(c.contains(&("profile-title".into(), format!("base64:{}", STANDARD.encode("Моя")))));
        assert!(c.iter().any(|(k, _)| k == "subscription-userinfo"));
        assert!(body_comments("[{}]").is_empty());
    }

    #[test]
    fn url_replacement() {
        let cur = "https://old.example/sub/abc";
        let info = |new_url: Option<&str>, new_domain: Option<&str>| SubInfo {
            new_url: new_url.map(String::from),
            new_domain: new_domain.map(String::from),
            ..Default::default()
        };
        assert_eq!(
            replacement_url(&info(Some("https://new.example/sub/x"), Some("d.example")), cur).as_deref(),
            Some("https://new.example/sub/x")
        );
        assert_eq!(replacement_url(&info(None, Some("new.example")), cur).as_deref(), Some("https://new.example/sub/abc"));
        assert_eq!(replacement_url(&info(Some("http://insecure.example/x"), None), cur), None);
        assert_eq!(replacement_url(&info(Some(cur), None), cur), None);
        assert_eq!(replacement_url(&info(None, None), cur), None);
        assert_eq!(decode_text("BASE64:0JzQvtGP"), "Моя");
        assert_eq!(decode_text("base64:!!!"), "!!!");
    }

    #[test]
    fn rejects_html() {
        assert!(parse_body("<!doctype html>").is_err());
    }

    #[test]
    fn json_endpoint_url() {
        assert_eq!(json_endpoint("https://p.example/sub/abc").as_deref(), Some("https://p.example/sub/abc/json"));
        assert_eq!(json_endpoint("https://p.example/sub/abc/").as_deref(), Some("https://p.example/sub/abc/json"));
        assert_eq!(json_endpoint("https://p.example/sub/abc?x=1").as_deref(), Some("https://p.example/sub/abc/json?x=1"));
        assert_eq!(json_endpoint("https://p.example/sub/abc/json"), None);
    }

    #[test]
    fn userinfo_and_text() {
        let i = parse_userinfo("upload=10; download=20; total=107374182400; expire=1798761600");
        assert_eq!((i.upload, i.download, i.total, i.expire), (Some(10), Some(20), Some(107374182400), Some(1798761600)));
        assert_eq!(parse_userinfo("upload=0; download=0; total=0; expire=0").total, None);
        assert_eq!(decode_text(&format!("base64:{}", STANDARD.encode("Дуализм"))), "Дуализм");
        assert_eq!(decode_text("Plain"), "Plain");
    }
}

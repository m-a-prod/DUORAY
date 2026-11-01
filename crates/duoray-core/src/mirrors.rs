//! Spare addresses of a subscription panel, published in DNS.
//!
//! A TXT record `_duoray.<panel host>` lists other hosts serving the same
//! subscriptions, separated by spaces (several TXT records add up):
//!
//! ```text
//! _duoray.vip.dualizm.space. TXT "sub2.dualizm.space https://mirror.example.org:8443"
//! ```
//!
//! When the panel does not answer, the subscription URL is tried with each
//! of them in place of its host; the path, with the user's token, stays. This
//! works on the very first fetch, unlike the panel's `fallback-url` header,
//! which is only known after a fetch succeeded. The record is read over DNS
//! over HTTPS first (plain DNS is blocked while the VPN's strict DNS is on,
//! and it is the part of DNS a network tampers with most), then over plain
//! DNS (Yandex's DoH speaks HTTP/2 only, which ureq does not).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use url::Url;

/// DoH endpoints (RFC 8484) answering over HTTP/1.1, tried in order.
const RESOLVERS: &[&str] = &["https://cloudflare-dns.com/dns-query", "https://dns.google/dns-query"];

/// Plain DNS after DoH: Yandex answers in Russia.
const UDP_RESOLVERS: &[&str] = &["77.88.8.8:53", "1.1.1.1:53"];

const CACHE_FOR: Duration = Duration::from_secs(600);

type Cache = HashMap<String, (Instant, Vec<String>)>;
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// `url` with its host replaced by each spare one from `_duoray.<host>`.
/// Empty when there is no record or no resolver answered.
pub fn spare_urls(url: &str, agent: &ureq::Agent) -> Vec<String> {
    let Ok(original) = Url::parse(url) else { return vec![] };
    let Some(host) = original.host_str().filter(|h| h.parse::<std::net::IpAddr>().is_err()) else { return vec![] };
    spare_hosts(host, agent).iter().filter_map(|spare| with_origin(&original, spare)).filter(|u| u != url).collect()
}

/// The spare hosts listed for `host`, cached for a while.
fn spare_hosts(host: &str, agent: &ureq::Agent) -> Vec<String> {
    if let Some((at, hosts)) = CACHE.lock().unwrap().get_or_insert_default().get(host)
        && at.elapsed() < CACHE_FOR
    {
        return hosts.clone();
    }
    let name = format!("_duoray.{host}");
    let Some(records) = RESOLVERS
        .iter()
        .find_map(|r| lookup_txt(agent, r, &name))
        .or_else(|| UDP_RESOLVERS.iter().find_map(|r| lookup_txt_udp(r, &name)))
    else {
        return vec![];
    };
    let hosts: Vec<String> = records.iter().flat_map(|r| r.split_whitespace()).map(String::from).collect();
    CACHE.lock().unwrap().get_or_insert_default().insert(host.to_string(), (Instant::now(), hosts.clone()));
    hosts
}

/// `original` served from `spare`: a host ("sub2.example") keeps the scheme
/// and port, an origin ("https://mirror.example:8443") sets them.
fn with_origin(original: &Url, spare: &str) -> Option<String> {
    let mut url = original.clone();
    if spare.contains("://") {
        let origin = Url::parse(spare).ok()?;
        if !matches!(origin.scheme(), "https" | "http") {
            return None;
        }
        url.set_scheme(origin.scheme()).ok()?;
        url.set_host(Some(origin.host_str()?)).ok()?;
        url.set_port(origin.port()).ok()?;
    } else {
        let (host, port) = match spare.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, Some(p.parse::<u16>().ok()?)),
            _ => (spare, url.port()),
        };
        url.set_host(Some(host)).ok()?;
        url.set_port(port).ok()?;
    }
    Some(url.to_string())
}

/// TXT strings of `name` from one DoH resolver; `None` if it did not answer
/// (an empty list is an answer: no record).
fn lookup_txt(agent: &ureq::Agent, resolver: &str, name: &str) -> Option<Vec<String>> {
    let query = txt_query(name)?;
    let mut resp = agent
        .post(resolver)
        .config()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .header("Content-Type", "application/dns-message")
        .header("Accept", "application/dns-message")
        .send(&query[..])
        .ok()?;
    let body = resp.body_mut().with_config().limit(64 * 1024).read_to_vec().ok()?;
    parse_txt_answer(&body)
}

/// TXT strings of `name` over plain DNS (UDP) from `resolver`.
fn lookup_txt_udp(resolver: &str, name: &str) -> Option<Vec<String>> {
    let query = txt_query(name)?;
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    socket.send_to(&query, resolver).ok()?;
    let mut buf = [0; 4096];
    let (n, _) = socket.recv_from(&mut buf).ok()?;
    parse_txt_answer(&buf[..n])
}

/// A DNS query (RFC 1035) for the TXT records of `name`, recursion desired.
fn txt_query(name: &str) -> Option<Vec<u8>> {
    // id 0 (RFC 8484 asks for it, for caching), RD, one question.
    let mut q = vec![0, 0, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.trim_end_matches('.').split('.') {
        let len = u8::try_from(label.len()).ok().filter(|l| (1..=63).contains(l))?;
        q.push(len);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 16, 0, 1]); // root, QTYPE TXT, QCLASS IN
    Some(q)
}

/// The TXT strings in a DNS response; `None` if it is malformed or an error
/// other than "no such name".
fn parse_txt_answer(msg: &[u8]) -> Option<Vec<String>> {
    let rcode = msg.get(3)? & 0x0f;
    if rcode == 3 {
        return Some(vec![]); // NXDOMAIN: no record
    }
    if rcode != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([*msg.get(4)?, *msg.get(5)?]);
    let ancount = u16::from_be_bytes([*msg.get(6)?, *msg.get(7)?]);
    let mut pos = 12;
    for _ in 0..qdcount {
        pos = skip_name(msg, pos)? + 4;
    }
    let mut records = vec![];
    for _ in 0..ancount {
        pos = skip_name(msg, pos)?;
        let rtype = u16::from_be_bytes([*msg.get(pos)?, *msg.get(pos + 1)?]);
        let rdlen = usize::from(u16::from_be_bytes([*msg.get(pos + 8)?, *msg.get(pos + 9)?]));
        let rdata = msg.get(pos + 10..pos + 10 + rdlen)?;
        pos += 10 + rdlen;
        if rtype != 16 {
            continue; // a CNAME on the way
        }
        // One or more <length><text> strings, joined into one record.
        let mut text = String::new();
        let mut i = 0;
        while i < rdata.len() {
            let len = usize::from(rdata[i]);
            text.push_str(&String::from_utf8_lossy(rdata.get(i + 1..i + 1 + len)?));
            i += 1 + len;
        }
        records.push(text);
    }
    Some(records)
}

/// Position after a (possibly compressed) domain name at `pos`.
fn skip_name(msg: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *msg.get(pos)?;
        match len {
            0 => return Some(pos + 1),
            l if l & 0xc0 == 0xc0 => return Some(pos + 2),
            l => pos += 1 + usize::from(l),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spare_host_replaces_only_the_host() {
        let url = Url::parse("https://vip.dualizm.space/sub/TOKEN?x=1").unwrap();
        assert_eq!(with_origin(&url, "sub2.dualizm.space").as_deref(), Some("https://sub2.dualizm.space/sub/TOKEN?x=1"));
        assert_eq!(with_origin(&url, "m.example:8443").as_deref(), Some("https://m.example:8443/sub/TOKEN?x=1"));
        assert_eq!(with_origin(&url, "http://m.example:8080").as_deref(), Some("http://m.example:8080/sub/TOKEN?x=1"));
        assert_eq!(with_origin(&url, "ftp://m.example"), None);
    }

    #[test]
    fn builds_a_txt_query() {
        let q = txt_query("_duoray.vip.dualizm.space").unwrap();
        assert_eq!(&q[12..20], b"\x07_duoray");
        assert_eq!(&q[q.len() - 5..], &[0, 0, 16, 0, 1]);
        assert!(txt_query(&format!("{}.x", "a".repeat(64))).is_none());
    }

    /// Asks the real resolvers (network): `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_resolvers_answer() {
        let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
        let missing = "_duoray.nonexistent-duoray-test.invalid";
        for r in RESOLVERS {
            let txt = lookup_txt(&agent, r, "_dmarc.google.com").unwrap_or_else(|| panic!("{r} did not answer"));
            assert!(txt.iter().any(|t| t.starts_with("v=DMARC1")), "{r}: {txt:?}");
            assert_eq!(lookup_txt(&agent, r, missing), Some(vec![]), "{r}");
        }
        for r in UDP_RESOLVERS {
            let txt = lookup_txt_udp(r, "_dmarc.google.com").unwrap_or_else(|| panic!("{r} did not answer"));
            assert!(txt.iter().any(|t| t.starts_with("v=DMARC1")), "{r}: {txt:?}");
            assert_eq!(lookup_txt_udp(r, missing), Some(vec![]), "{r}");
        }
    }

    #[test]
    fn parses_txt_answers() {
        // Header: 1 question, 2 answers (a CNAME, then a TXT of two strings).
        let mut msg = vec![0, 0, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0];
        msg.extend(txt_query("_duoray.a.b").unwrap()[12..].iter());
        // CNAME with a compressed name pointing at the question.
        msg.extend([0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 12]);
        let strings: &[&[u8]] = &[b"sub2.example ", b"https://m.example:8443"];
        let rdata: Vec<u8> = strings.iter().flat_map(|s| std::iter::once(s.len() as u8).chain(s.iter().copied())).collect();
        msg.extend([0xc0, 12, 0, 16, 0, 1, 0, 0, 0, 60, 0, rdata.len() as u8]);
        msg.extend(rdata);
        assert_eq!(parse_txt_answer(&msg), Some(vec!["sub2.example https://m.example:8443".to_string()]));

        let mut nx = vec![0, 0, 0x81, 0x83, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_txt_answer(&nx), Some(vec![]), "NXDOMAIN: no spare hosts");
        nx[3] = 0x82;
        assert_eq!(parse_txt_answer(&nx), None, "SERVFAIL: ask the next resolver");
        assert_eq!(parse_txt_answer(&[0, 1]), None);
    }
}

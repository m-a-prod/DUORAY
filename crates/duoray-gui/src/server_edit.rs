//! Server editor model: the form for share-link servers and the JSON helpers.
//! JSON servers are edited as text; link servers as fields grouped like the
//! link itself (connection, transport, security), showing only the fields
//! that apply to the chosen transport and security.

use duoray_core::link::{Profile, Protocol};
use serde_json::{Map, Value, json};

/// Shown in option lists for "not set".
const NONE: &str = "—";

pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub value: String,
    /// Non-empty: pick from these instead of typing.
    pub options: &'static [&'static str],
    /// Section title, set on the first field of a section.
    pub header: &'static str,
    pub placeholder: &'static str,
}

/// Fields whose change shows or hides other fields.
pub fn changes_layout(key: &str) -> bool {
    matches!(key, "network" | "security" | "header_type")
}

/// Collects fields; the pending section title goes on the next one added.
#[derive(Default)]
struct Fields {
    out: Vec<Field>,
    header: &'static str,
}

impl Fields {
    fn section(&mut self, title: &'static str) {
        self.header = title;
    }

    fn add(
        &mut self,
        key: &'static str,
        label: &'static str,
        value: &str,
        options: &'static [&'static str],
        placeholder: &'static str,
    ) {
        let value = if value.is_empty() && options.contains(&NONE) { NONE.to_string() } else { value.to_string() };
        let header = std::mem::take(&mut self.header);
        self.out.push(Field { key, label, value, options, header, placeholder });
    }
}

pub fn fields(p: &Profile) -> Vec<Field> {
    let s = &p.stream;
    let mut f = Fields::default();
    f.section("ПОДКЛЮЧЕНИЕ");

    f.add("name", "Название", &p.name, &[], "");
    f.add("address", "Адрес", &p.address, &[], "example.com или 1.2.3.4");
    f.add("port", "Порт", &p.port.to_string(), &[], "443");
    match &p.protocol {
        Protocol::Vless { id, flow, encryption } => {
            f.add("id", "UUID", id, &[], "");
            f.add("flow", "Flow", flow, &[NONE, "xtls-rprx-vision", "xtls-rprx-vision-udp443"], "");
            f.add("encryption", "Шифрование", encryption, &[], "none");
        }
        Protocol::Vmess { id, alter_id, cipher } => {
            f.add("id", "UUID", id, &[], "");
            f.add("alter_id", "alterId", &alter_id.to_string(), &[], "0");
            f.add("cipher", "Шифрование", cipher, &["auto", "aes-128-gcm", "chacha20-poly1305", "none", "zero"], "");
        }
        Protocol::Trojan { password } => f.add("password", "Пароль", password, &[], ""),
        Protocol::Shadowsocks { method, password } => {
            f.add(
                "method",
                "Метод",
                method,
                &[
                    "2022-blake3-aes-128-gcm",
                    "2022-blake3-aes-256-gcm",
                    "2022-blake3-chacha20-poly1305",
                    "aes-128-gcm",
                    "aes-256-gcm",
                    "chacha20-ietf-poly1305",
                    "xchacha20-ietf-poly1305",
                    "none",
                ],
                "",
            );
            f.add("password", "Пароль", password, &[], "");
        }
    }

    f.section("ТРАНСПОРТ");
    let network = if s.network.is_empty() { "tcp" } else { s.network.as_str() };
    f.add("network", "Тип", network, &["tcp", "ws", "grpc", "xhttp", "httpupgrade", "kcp"], "");
    match network {
        "ws" | "httpupgrade" => {
            f.add("host", "Host", &s.host, &[], "по умолчанию — адрес сервера");
            f.add("path", "Path", &s.path, &[], "/");
        }
        "xhttp" => {
            f.add("host", "Host", &s.host, &[], "по умолчанию — адрес сервера");
            f.add("path", "Path", &s.path, &[], "/");
            let mode = if s.mode.is_empty() { "auto" } else { s.mode.as_str() };
            f.add("mode", "Режим", mode, &["auto", "packet-up", "stream-up", "stream-one"], "");
        }
        "grpc" => {
            f.add("service_name", "serviceName", &s.service_name, &[], "");
            f.add("mode", "Режим", if s.mode == "multi" { "multi" } else { "gun" }, &["gun", "multi"], "");
        }
        "tcp" => {
            let header_type = if s.header_type == "http" { "http" } else { "none" };
            f.add("header_type", "Маскировка", header_type, &["none", "http"], "");
            if header_type == "http" {
                f.add("host", "Host", &s.host, &[], "");
                f.add("path", "Path", &s.path, &[], "/");
            }
        }
        _ => {}
    }

    f.section("БЕЗОПАСНОСТЬ");
    let security = if s.security.is_empty() { "none" } else { s.security.as_str() };
    f.add("security", "Тип", security, &["none", "tls", "reality"], "");
    const FINGERPRINTS: &[&str] =
        &["chrome", "firefox", "safari", "ios", "android", "edge", "360", "qq", "random", "randomized"];
    match security {
        "tls" => {
            f.add("sni", "SNI", &s.sni, &[], "по умолчанию — адрес сервера");
            f.add("fingerprint", "Fingerprint", if s.fingerprint.is_empty() { "chrome" } else { &s.fingerprint }, FINGERPRINTS, "");
            f.add("alpn", "ALPN", &s.alpn.join(", "), &[], "h2, http/1.1");
            f.add("allow_insecure", "allowInsecure", if s.allow_insecure { "да" } else { "нет" }, &["нет", "да"], "");
        }
        "reality" => {
            f.add("sni", "SNI", &s.sni, &[], "");
            f.add("fingerprint", "Fingerprint", if s.fingerprint.is_empty() { "chrome" } else { &s.fingerprint }, FINGERPRINTS, "");
            f.add("public_key", "Public key", &s.public_key, &[], "");
            f.add("short_id", "Short ID", &s.short_id, &[], "");
            f.add("spider_x", "SpiderX", &s.spider_x, &[], "");
        }
        _ => {}
    }
    f.out
}

/// Writes one field back. Numbers are checked by [`validate`], so an
/// unparsable port keeps the previous value here.
pub fn set(p: &mut Profile, key: &str, value: &str) {
    let v = if value == NONE { "" } else { value.trim() };
    let s = &mut p.stream;
    match (key, &mut p.protocol) {
        ("name", _) => p.name = value.to_string(),
        ("address", _) => p.address = v.into(),
        ("port", _) => p.port = v.parse().unwrap_or(0),
        ("id", Protocol::Vless { id, .. } | Protocol::Vmess { id, .. }) => *id = v.into(),
        ("flow", Protocol::Vless { flow, .. }) => *flow = v.into(),
        ("encryption", Protocol::Vless { encryption, .. }) => *encryption = v.into(),
        ("alter_id", Protocol::Vmess { alter_id, .. }) => *alter_id = v.parse().unwrap_or(u32::MAX),
        ("cipher", Protocol::Vmess { cipher, .. }) => *cipher = v.into(),
        ("password", Protocol::Trojan { password } | Protocol::Shadowsocks { password, .. }) => *password = v.into(),
        ("method", Protocol::Shadowsocks { method, .. }) => *method = v.into(),
        ("network", _) => s.network = v.into(),
        ("host", _) => s.host = v.into(),
        ("path", _) => s.path = v.into(),
        ("mode", _) => s.mode = v.into(),
        ("service_name", _) => s.service_name = v.into(),
        ("header_type", _) => s.header_type = v.into(),
        ("security", _) => s.security = v.into(),
        ("sni", _) => s.sni = v.into(),
        ("fingerprint", _) => s.fingerprint = v.into(),
        ("alpn", _) => s.alpn = v.split(',').map(str::trim).filter(|a| !a.is_empty()).map(String::from).collect(),
        ("allow_insecure", _) => s.allow_insecure = v == "да",
        ("public_key", _) => s.public_key = v.into(),
        ("short_id", _) => s.short_id = v.into(),
        ("spider_x", _) => s.spider_x = v.into(),
        _ => {}
    }
}

/// First problem as (field key, message).
pub fn validate(p: &Profile) -> Result<(), (&'static str, &'static str)> {
    if p.name.trim().is_empty() {
        return Err(("name", "Укажите название"));
    }
    if p.address.is_empty() || p.address.contains(char::is_whitespace) {
        return Err(("address", "Укажите адрес сервера"));
    }
    if p.port == 0 {
        return Err(("port", "Порт — число от 1 до 65535"));
    }
    match &p.protocol {
        Protocol::Vless { id, .. } | Protocol::Vmess { id, .. } if id.is_empty() => {
            return Err(("id", "Укажите UUID"));
        }
        Protocol::Vmess { alter_id, .. } if *alter_id == u32::MAX => return Err(("alter_id", "alterId — целое число")),
        Protocol::Trojan { password } | Protocol::Shadowsocks { password, .. } if password.is_empty() => {
            return Err(("password", "Укажите пароль"));
        }
        _ => {}
    }
    if p.stream.security == "reality" && p.stream.public_key.is_empty() {
        return Err(("public_key", "Для Reality нужен public key"));
    }
    Ok(())
}

/// The config a link server runs with, named after it, ready to be edited as
/// JSON (and then saved as a JSON server).
pub fn link_to_json(p: &Profile) -> Value {
    let mut config = Map::new();
    config.insert("remarks".into(), json!(p.name));
    if let Some(d) = &p.description {
        config.insert("meta".into(), json!({ "serverDescription": d }));
    }
    if let Value::Object(rest) = duoray_core::xray::config_from_profile(p) {
        config.extend(rest);
    }
    Value::Object(config)
}

pub fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Highlight layers, in the order the editor colors them.
pub mod layer {
    pub const PUNCT: u8 = 0;
    pub const KEY: u8 = 1;
    pub const STRING: u8 = 2;
    /// Numbers, true / false / null.
    pub const NUMBER: u8 = 3;
    /// xray enumerations (protocol, network, security) and rule prefixes (geosite:, geoip:, …).
    pub const ACCENT: u8 = 4;
    /// Outbound / inbound / balancer tags, so references are easy to match up.
    pub const TAG: u8 = 5;
    /// Bare words that are not JSON.
    pub const BAD: u8 = 6;
}

/// Above this size the editor shows plain text: every keystroke re-lays out
/// each layer.
const HIGHLIGHT_LIMIT: usize = 256 * 1024;

const TAG_KEYS: &[&str] =
    &["tag", "outboundTag", "inboundTag", "balancerTag", "dialerProxy", "selector", "fallbackTag"];
const ENUM_KEYS: &[&str] = &["protocol", "network", "security", "domainStrategy", "loglevel", "flow", "mode"];
const RULE_PREFIXES: &[&str] =
    &["geosite:", "geoip:", "domain:", "full:", "regexp:", "keyword:", "ext:", "ext-domain:", "ext-ip:"];

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Punct(u8),
    Str,
    Num,
    Word,
}

/// Splits (possibly broken) JSON into tokens; never fails, so it keeps
/// working while the user types. An unterminated string ends at the line end.
fn lex(text: &str) -> Vec<(usize, usize, Tok)> {
    let b = text.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let tok = match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            c @ (b'{' | b'}' | b'[' | b']' | b',' | b':') => {
                i += 1;
                Tok::Punct(c)
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                    i += if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] != b'\n' { 2 } else { 1 };
                }
                if i < b.len() && b[i] == b'"' {
                    i += 1;
                }
                Tok::Str
            }
            b'-' | b'0'..=b'9' => {
                i += 1;
                while i < b.len() && matches!(b[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    i += 1;
                }
                Tok::Num
            }
            _ => {
                while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | b'{' | b'}' | b'[' | b']' | b',' | b':' | b'"') {
                    i += 1;
                }
                Tok::Word
            }
        };
        out.push((start, i, tok));
    }
    out
}

/// A run of one color within a line; `col` counts characters from the line start.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub col: usize,
    pub text: String,
    pub kind: u8,
}

/// Colored spans per line. The editor draws them at `col × character width`
/// over the (transparent) text, so in a monospace font they cover it glyph for
/// glyph, and a keystroke only changes the spans of the lines it touched.
/// `None` when the text is too big to highlight.
pub fn highlight(text: &str) -> Option<Vec<Vec<Span>>> {
    if text.len() > HIGHLIGHT_LIMIT {
        return None;
    }
    let mut class = vec![layer::PUNCT; text.len()];
    let tokens = lex(text);
    // Key that opened each open container (arrays hand it to their items).
    let mut stack: Vec<(bool, Option<&str>)> = vec![];
    let mut key: Option<&str> = None;
    for (n, &(start, end, tok)) in tokens.iter().enumerate() {
        let mut paint = |from: usize, to: usize, k: u8| class[from..to].fill(k);
        match tok {
            Tok::Punct(c @ (b'{' | b'[')) => stack.push((c == b'[', key.take())),
            Tok::Punct(b'}' | b']') => {
                stack.pop();
                key = None;
            }
            Tok::Punct(b',') => {
                if !stack.last().is_some_and(|s| s.0) {
                    key = None;
                }
            }
            Tok::Punct(_) => {}
            Tok::Str if matches!(tokens.get(n + 1), Some((_, _, Tok::Punct(b':')))) => {
                paint(start, end, layer::KEY);
                key = Some(text[start..end].trim_matches('"'));
            }
            Tok::Str => {
                let owner = match stack.last() {
                    Some((true, k)) => *k,
                    _ => key,
                };
                let inner = text[start..end].trim_start_matches('"');
                let k = match owner {
                    Some(o) if TAG_KEYS.contains(&o) => layer::TAG,
                    Some(o) if ENUM_KEYS.contains(&o) => layer::ACCENT,
                    _ => layer::STRING,
                };
                paint(start, end, k);
                if k == layer::STRING
                    && let Some(p) = RULE_PREFIXES.iter().find(|p| inner.starts_with(**p))
                {
                    paint(start + 1, start + 1 + p.len(), layer::ACCENT);
                }
            }
            Tok::Num => paint(start, end, layer::NUMBER),
            Tok::Word if matches!(&text[start..end], "true" | "false" | "null") => paint(start, end, layer::NUMBER),
            Tok::Word => paint(start, end, layer::BAD),
        }
    }

    let mut lines = vec![];
    let mut offset = 0;
    for line in text.split('\n') {
        let mut spans: Vec<Span> = vec![];
        for (col, (i, c)) in line.char_indices().enumerate() {
            let kind = class[offset + i];
            match spans.last_mut() {
                Some(s) if s.kind == kind && s.col + s.text.chars().count() == col => s.text.push(c),
                _ if c.is_whitespace() => {}
                _ => spans.push(Span { col, text: c.into(), kind }),
            }
        }
        for s in &mut spans {
            s.text.truncate(s.text.trim_end().len());
        }
        lines.push(spans);
        offset += line.len() + 1;
    }
    Some(lines)
}

/// A monospace family the platform is sure to have. Slint wants a real family
/// name, so on Linux ask fontconfig what "monospace" means there.
pub fn mono_font() -> String {
    if cfg!(target_os = "windows") {
        return "Consolas".into();
    }
    if cfg!(target_os = "macos") {
        return "Menlo".into();
    }
    std::process::Command::new("fc-match")
        .args(["-f", "%{family[0]}", "monospace"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "DejaVu Sans Mono".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Texts of all spans of `kind`, in order.
    fn ink(lines: &[Vec<Span>], kind: u8) -> String {
        lines.iter().flatten().filter(|s| s.kind == kind).map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn highlights_xray() {
        let text = "{\n  \"protocol\": \"vless\",\n  \"port\": 443, \"ok\": true,\n  \"outboundTag\": \"proxy\",\n  \"domain\": [\"geosite:ru\", \"x.com\"],\n  \"selector\": [\"a\"], oops\n  \"remarks\": \"🇩🇪 Б\"\n}";
        let l = highlight(text).unwrap();
        assert_eq!(l.len(), text.lines().count());
        // Spans sit where their text is.
        for (line, spans) in text.lines().zip(&l) {
            let chars: Vec<char> = line.chars().collect();
            for s in spans {
                let at: String = chars[s.col..s.col + s.text.chars().count()].iter().collect();
                assert_eq!(at, s.text);
            }
        }
        assert_eq!(ink(&l, layer::KEY), r#""protocol" "port" "ok" "outboundTag" "domain" "selector" "remarks""#);
        assert_eq!(ink(&l, layer::ACCENT), r#""vless" geosite:"#);
        assert_eq!(ink(&l, layer::TAG), r#""proxy" "a""#);
        assert_eq!(ink(&l, layer::STRING), r#"" ru" "x.com" "🇩🇪 Б""#);
        assert_eq!(l[1][0], Span { col: 2, text: r#""protocol""#.into(), kind: layer::KEY });
        assert_eq!(ink(&l, layer::NUMBER), "443 true");
        assert_eq!(ink(&l, layer::BAD), "oops");
        // Typing an unterminated string must not swallow the next lines.
        let l = highlight("{\"a\": \"b,\n\"c\": 1}").unwrap();
        assert_eq!((ink(&l, layer::KEY).as_str(), ink(&l, layer::STRING).as_str()), (r#""a" "c""#, r#""b,"#));
        assert!(highlight(&"x".repeat(HIGHLIGHT_LIMIT + 1)).is_none());
    }

    #[test]
    fn form_roundtrip() {
        let mut p = duoray_core::link::parse(
            "vless://u-1@h.example:443?type=ws&security=tls&path=%2Fws&sni=s.example#Name",
        )
        .unwrap();
        let keys: Vec<_> = fields(&p).iter().map(|f| f.key).collect();
        assert!(keys.contains(&"path") && keys.contains(&"sni") && !keys.contains(&"public_key"));
        assert_eq!(fields(&p).iter().find(|f| f.key == "flow").unwrap().value, NONE);

        set(&mut p, "security", "reality");
        assert_eq!(validate(&p), Err(("public_key", "Для Reality нужен public key")));
        set(&mut p, "public_key", " pk ");
        set(&mut p, "flow", "xtls-rprx-vision");
        set(&mut p, "network", "tcp");
        assert_eq!(validate(&p), Ok(()));
        assert!(fields(&p).iter().any(|f| f.key == "short_id"));
        set(&mut p, "port", "70000");
        assert_eq!(validate(&p), Err(("port", "Порт — число от 1 до 65535")));

        let config = link_to_json(&p);
        assert_eq!(config.as_object().unwrap().keys().next().map(String::as_str), Some("remarks"));
        let s = duoray_core::server::Server::from_json_text(&pretty(&config)).unwrap();
        assert_eq!((s.name.as_str(), s.security.as_str()), ("Name", "reality"));
    }
}

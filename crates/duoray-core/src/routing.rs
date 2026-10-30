//! User routing on top of the subscription's own: the simple switches
//! ("Russian sites direct", "games direct", ...) and the advanced profiles.
//!
//! Our rules go after the panel's service rules (its internal DNS inbound and
//! the port 53 rule), and before everything else the panel routes, so the
//! user's choice wins while the subscription keeps working as its author set
//! it up. Direct traffic leaves through our own `duoray-direct` outbound bound
//! to the physical interface, so it never depends on the panel's tag names.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DIRECT_TAG: &str = "duoray-direct";
pub const BLOCK_TAG: &str = "duoray-block";
/// Second SOCKS inbound: everything that arrives here goes direct. Duotun sends
/// the connections of apps that bypass the VPN to it.
pub const DIRECT_INBOUND: &str = "socks-direct";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    Proxy,
    Direct,
    Block,
}

impl Action {
    pub const ALL: [Action; 3] = [Action::Proxy, Action::Direct, Action::Block];

    pub fn title(self) -> &'static str {
        match self {
            Action::Proxy => "Через VPN",
            Action::Direct => "Напрямую",
            Action::Block => "Блокировать",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingSettings {
    /// Advanced mode: profiles with custom rules instead of the switches.
    pub advanced: bool,
    pub simple: SimpleRouting,
    pub profiles: Vec<RouteProfile>,
    pub active_profile: usize,
    /// Per-app routing; applies in both modes.
    pub apps: AppRouting,
}

impl Default for RoutingSettings {
    fn default() -> Self {
        Self {
            advanced: false,
            simple: SimpleRouting::default(),
            profiles: vec![RouteProfile::default()],
            active_profile: 0,
            apps: AppRouting::default(),
        }
    }
}

impl RoutingSettings {
    pub fn profile(&self) -> Option<&RouteProfile> {
        self.profiles.get(self.active_profile).or(self.profiles.first())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimpleRouting {
    pub lan_direct: bool,
    /// All Russian IPs and Russian domains.
    pub ru_direct: bool,
    /// The mobile-internet whitelist (hxehex): domains and IPs that stay
    /// reachable when the operator restricts everything else.
    pub whitelist_direct: bool,
    /// Ids from [`GAMES`].
    pub games: Vec<String>,
}

impl Default for SimpleRouting {
    fn default() -> Self {
        Self { lan_direct: true, ru_direct: false, whitelist_direct: false, games: vec![] }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteProfile {
    pub name: String,
    pub rules: Vec<RouteRule>,
    /// Where traffic no rule matched goes. `Proxy` leaves it to the panel.
    pub default_action: Action,
    /// Keep the subscription's own rules after ours.
    pub keep_panel_rules: bool,
    /// xray `domainStrategy`; empty keeps the panel's.
    pub domain_strategy: String,
}

impl Default for RouteProfile {
    fn default() -> Self {
        Self {
            name: "Мой профиль".into(),
            rules: vec![],
            default_action: Action::Proxy,
            keep_panel_rules: true,
            domain_strategy: String::new(),
        }
    }
}

/// One rule as the user typed it: lists are free text (one entry per line,
/// or separated by commas/spaces), parsed when the config is built.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteRule {
    pub enabled: bool,
    pub action: Action,
    /// `example.com`, `domain:`, `full:`, `keyword:`, `regexp:`, `geosite:`.
    pub domains: String,
    /// IPs, CIDRs, `geoip:`.
    pub ips: String,
    /// `443`, `1000-2000`, `80,443`.
    pub ports: String,
    /// "", "tcp", "udp".
    pub network: String,
}

impl Default for RouteRule {
    fn default() -> Self {
        Self {
            enabled: true,
            action: Action::Direct,
            domains: String::new(),
            ips: String::new(),
            ports: String::new(),
            network: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppMode {
    #[default]
    Off,
    /// The listed apps bypass the VPN.
    Bypass,
    /// Only the listed apps use the VPN.
    Only,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppRouting {
    pub mode: AppMode,
    /// Process names (`steam.exe`, `Telegram`) or full paths.
    pub apps: Vec<String>,
}

impl AppRouting {
    pub fn active(&self) -> bool {
        self.mode != AppMode::Off && (self.mode == AppMode::Only || !self.apps.is_empty())
    }
}

pub struct Game {
    pub id: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
    pub geosite: &'static [&'static str],
    /// Name of an IP list in [`GeoData::lists`] (game servers have no domain).
    pub ips: Option<&'static str>,
}

pub const GAMES: &[Game] = &[
    Game {
        id: "steam",
        title: "Steam и CS2",
        subtitle: "Матчи CS2 и Dota 2 (серверы Valve)",
        // Game traffic only. The store, community and CDNs stay in the VPN:
        // their Russian Akamai edges block IPs that send many requests.
        geosite: &[],
        ips: Some("valve"),
    },
    Game { id: "faceit", title: "FACEIT", subtitle: "Клиент и серверы FACEIT", geosite: &["faceit"], ips: None },
    Game {
        id: "riot",
        title: "Riot Games",
        subtitle: "Valorant, League of Legends",
        geosite: &["riot"],
        ips: Some("riot"),
    },
    Game {
        id: "blizzard",
        title: "Battle.net",
        subtitle: "Overwatch, WoW, Diablo, Call of Duty",
        geosite: &["blizzard"],
        ips: Some("blizzard"),
    },
    Game { id: "epic", title: "Epic Games", subtitle: "Fortnite и магазин Epic", geosite: &["epicgames"], ips: None },
    Game { id: "ea", title: "EA", subtitle: "EA app, Apex Legends, EA FC", geosite: &["ea", "origin"], ips: None },
    Game { id: "ubisoft", title: "Ubisoft", subtitle: "Ubisoft Connect", geosite: &["ubisoft"], ips: None },
];

/// Lists shipped with the app; [`GeoData`] replaces them with fresh copies.
pub const BUILTIN_LISTS: &[(&str, &str)] = &[
    ("valve", include_str!("../assets/routing/valve.txt")),
    ("riot", include_str!("../assets/routing/riot.txt")),
    ("blizzard", include_str!("../assets/routing/blizzard.txt")),
    ("whitelist-domains", include_str!("../assets/routing/whitelist-domains.txt")),
];

/// What the rules may reference: tags present in the geo files xray will load
/// (a missing tag makes xray refuse to start) and the plain lists.
#[derive(Debug, Clone, Default)]
pub struct GeoData {
    pub geoip: HashSet<String>,
    pub geosite: HashSet<String>,
    pub lists: BTreeMap<String, Vec<String>>,
}

impl GeoData {
    /// Reads the tags of `geoip.dat`/`geosite.dat` in `assets` and the lists in
    /// `lists_dir` (`<name>.txt`), falling back to the built-in lists.
    pub fn load(assets: Option<&Path>, lists_dir: Option<&Path>) -> Self {
        let tags = |file: &str| {
            assets
                .and_then(|d| std::fs::read(d.join(file)).ok())
                .map(|d| dat_tags(&d))
                .unwrap_or_default()
        };
        let mut lists = BTreeMap::new();
        for (name, builtin) in BUILTIN_LISTS {
            let fresh = lists_dir.and_then(|d| std::fs::read_to_string(d.join(format!("{name}.txt"))).ok());
            let parsed = parse_list(fresh.as_deref().unwrap_or(builtin));
            let parsed = if parsed.is_empty() { parse_list(builtin) } else { parsed };
            lists.insert(name.to_string(), parsed);
        }
        Self { geoip: tags("geoip.dat"), geosite: tags("geosite.dat"), lists }
    }

    fn list(&self, name: &str) -> &[String] {
        self.lists.get(name).map(Vec::as_slice).unwrap_or_default()
    }
}

/// Non-empty lines without `#` comments.
pub fn parse_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or_default().trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *data.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

enum ProtoField<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn next_field<'a>(data: &'a [u8], pos: &mut usize) -> Option<(u32, ProtoField<'a>)> {
    let key = varint(data, pos)?;
    let number = (key >> 3) as u32;
    let field = match key & 7 {
        0 => ProtoField::Varint(varint(data, pos)?),
        1 | 2 | 5 => {
            let len = match key & 7 {
                1 => 8,
                5 => 4,
                _ => usize::try_from(varint(data, pos)?).ok()?,
            };
            let end = pos.checked_add(len)?;
            let bytes = data.get(*pos..end)?;
            *pos = end;
            ProtoField::Bytes(bytes)
        }
        _ => return None,
    };
    Some((number, field))
}

fn entries(mut data: &[u8], mut f: impl FnMut(&[u8])) {
    while !data.is_empty() {
        let mut pos = 0;
        let Some((1, ProtoField::Bytes(entry))) = next_field(data, &mut pos) else { break };
        f(entry);
        data = &data[pos..];
    }
}

/// Lower-cased tags of a v2ray `geoip.dat`/`geosite.dat`.
pub fn dat_tags(data: &[u8]) -> HashSet<String> {
    let mut tags = HashSet::new();
    entries(data, |entry| {
        let mut pos = 0;
        while let Some((number, field)) = next_field(entry, &mut pos) {
            if let (1, ProtoField::Bytes(tag)) = (number, field) {
                tags.insert(String::from_utf8_lossy(tag).to_lowercase());
                break;
            }
        }
    });
    tags
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoMatch {
    pub tag: String,
    pub value: String,
}

/// Which file a query asks for: rule syntax ("geoip:ru", "geosite:google")
/// names one, the bare query searches both.
pub fn split_geo_query(query: &str) -> (Option<&'static str>, &str) {
    let query = query.trim();
    for source in ["geosite", "geoip"] {
        if let Some(rest) = query.get(..source.len() + 1).filter(|p| p.eq_ignore_ascii_case(&format!("{source}:"))) {
            return (Some(source), query[rest.len()..].trim());
        }
    }
    (None, query)
}

/// Whether `cidr` ("1.2.3.0/24") contains `ip`.
fn cidr_contains(cidr: &str, ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let Some((net, prefix)) = cidr.split_once('/') else { return false };
    let (Ok(net), Ok(prefix)) = (net.parse::<IpAddr>(), prefix.parse::<u32>()) else { return false };
    let (net, ip, bits) = match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) => (u128::from(u32::from(n)), u128::from(u32::from(i)), 32),
        (IpAddr::V6(n), IpAddr::V6(i)) => (u128::from(n), u128::from(i), 128),
        _ => return false,
    };
    if prefix > bits {
        return false;
    }
    let shift = bits - prefix;
    shift >= 128 || (net >> shift) == (ip >> shift)
}

fn search_dat(
    data: &[u8],
    query: &str,
    limit: usize,
    parse: impl Fn(&[u8]) -> (String, Vec<String>),
    also: impl Fn(&str) -> bool,
) -> Vec<GeoMatch> {
    let query = query.trim().to_lowercase();
    let mut matches = Vec::new();
    if query.is_empty() || limit == 0 {
        return matches;
    }
    entries(data, |entry| {
        if matches.len() >= limit {
            return;
        }
        let (tag, values) = parse(entry);
        let tag_match = tag.to_lowercase().contains(&query);
        for value in values {
            if tag_match || value.to_lowercase().contains(&query) || also(&value) {
                matches.push(GeoMatch { tag: tag.clone(), value });
                if matches.len() >= limit {
                    break;
                }
            }
        }
    });
    matches
}

/// Searches domain values and tags in a v2ray `geosite.dat`.
pub fn search_geosite(data: &[u8], query: &str, limit: usize) -> Vec<GeoMatch> {
    search_dat(data, query, limit, geosite_entry, |_| false)
}

fn geosite_entry(entry: &[u8]) -> (String, Vec<String>) {
    let mut tag = String::new();
    let mut values = Vec::new();
    let mut pos = 0;
    while let Some((number, field)) = next_field(entry, &mut pos) {
        match (number, field) {
            (1, ProtoField::Bytes(value)) => tag = String::from_utf8_lossy(value).into_owned(),
            (2, ProtoField::Bytes(domain)) => {
                let mut kind = 0;
                let mut value = None;
                let mut pos = 0;
                while let Some((number, field)) = next_field(domain, &mut pos) {
                    match (number, field) {
                        (1, ProtoField::Varint(v)) => kind = v,
                        (2, ProtoField::Bytes(v)) => value = Some(String::from_utf8_lossy(v).into_owned()),
                        _ => {}
                    }
                }
                if let Some(value) = value {
                    let kind = match kind {
                        0 => "keyword".to_string(),
                        1 => "regexp".to_string(),
                        2 => "domain".to_string(),
                        3 => "full".to_string(),
                        other => format!("type-{other}"),
                    };
                    values.push(format!("{kind}:{value}"));
                }
            }
            _ => {}
        }
    }
    (tag, values)
}

/// Searches CIDR values and tags in a v2ray `geoip.dat`; an IP address finds
/// the networks that contain it.
pub fn search_geoip(data: &[u8], query: &str, limit: usize) -> Vec<GeoMatch> {
    let ip = query.trim().parse::<std::net::IpAddr>().ok();
    search_dat(data, query, limit, geoip_entry, |value| ip.is_some_and(|ip| cidr_contains(value, ip)))
}

fn geoip_entry(entry: &[u8]) -> (String, Vec<String>) {
    let mut tag = String::new();
    let mut values = Vec::new();
    let mut pos = 0;
    while let Some((number, field)) = next_field(entry, &mut pos) {
        match (number, field) {
            (1, ProtoField::Bytes(value)) => tag = String::from_utf8_lossy(value).into_owned(),
            (2, ProtoField::Bytes(cidr)) => {
                let mut ip = None;
                let mut prefix = 0;
                let mut pos = 0;
                while let Some((number, field)) = next_field(cidr, &mut pos) {
                    match (number, field) {
                        (1, ProtoField::Bytes(v)) => ip = Some(v),
                        (2, ProtoField::Varint(v)) => prefix = v,
                        _ => {}
                    }
                }
                let ip = ip.and_then(|ip| match ip.len() {
                    4 => <[u8; 4]>::try_from(ip).ok().map(std::net::Ipv4Addr::from).map(std::net::IpAddr::from),
                    16 => <[u8; 16]>::try_from(ip).ok().map(std::net::Ipv6Addr::from).map(std::net::IpAddr::from),
                    _ => None,
                });
                if let Some(ip) = ip {
                    values.push(format!("{ip}/{prefix}"));
                }
            }
            _ => {}
        }
    }
    (tag, values)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Outbound(String),
    Balancer(String),
}

impl Target {
    fn apply(&self, rule: &mut Value) {
        match self {
            Target::Outbound(t) => rule["outboundTag"] = json!(t),
            Target::Balancer(t) => rule["balancerTag"] = json!(t),
        }
    }
}

/// Our rules for the current settings, without the panel's. Entries that
/// reference unknown geo tags are dropped and reported in `warnings`.
pub fn user_rules(s: &RoutingSettings, geo: &GeoData, proxy: &Target, warnings: &mut Vec<String>) -> Vec<Value> {
    let direct = Target::Outbound(DIRECT_TAG.into());
    let mut out = vec![];
    if s.advanced {
        let Some(p) = s.profile() else { return out };
        for r in p.rules.iter().filter(|r| r.enabled) {
            let target = match r.action {
                Action::Proxy => proxy.clone(),
                Action::Direct => direct.clone(),
                Action::Block => Target::Outbound(BLOCK_TAG.into()),
            };
            let domains = split_entries(&r.domains).into_iter().map(normalize_domain).collect();
            let ips = split_entries(&r.ips);
            let mut rule = json!({ "type": "field" });
            let mut any = false;
            if let Some(d) = checked(domains, geo, warnings) {
                rule["domain"] = json!(d);
                any = true;
            }
            if let Some(i) = checked(ips, geo, warnings) {
                rule["ip"] = json!(i);
                any = true;
            }
            let ports = r.ports.split_whitespace().collect::<Vec<_>>().join("");
            if !ports.is_empty() {
                rule["port"] = json!(ports.trim_matches(','));
                any = true;
            }
            if matches!(r.network.as_str(), "tcp" | "udp") {
                rule["network"] = json!(r.network);
                any = true;
            }
            if any {
                target.apply(&mut rule);
                out.push(rule);
            }
        }
        return out;
    }

    let mut push = |domains: Vec<String>, ips: Vec<String>, warnings: &mut Vec<String>| {
        if let Some(d) = checked(domains, geo, warnings) {
            out.push(json!({ "type": "field", "domain": d, "outboundTag": DIRECT_TAG }));
        }
        if let Some(i) = checked(ips, geo, warnings) {
            out.push(json!({ "type": "field", "ip": i, "outboundTag": DIRECT_TAG }));
        }
    };
    let simple = &s.simple;
    if simple.lan_direct {
        push(vec![], vec!["geoip:private".into()], warnings);
    }
    if simple.whitelist_direct {
        let domains = geo.list("whitelist-domains").iter().map(|d| format!("domain:{d}")).collect();
        push(domains, vec!["geoip:ru-whitelist".into()], warnings);
    }
    if simple.ru_direct {
        let mut domains: Vec<String> = ["geosite:category-ru", "geosite:category-gov-ru"].map(String::from).into();
        // .ru .su .рф .рус .москва
        domains.extend(["ru", "su", "xn--p1ai", "xn--p1acf", "xn--80adxhks"].map(|z| format!("domain:{z}")));
        push(domains, vec!["geoip:ru".into()], warnings);
    }
    for game in GAMES.iter().filter(|g| simple.games.iter().any(|id| id == g.id)) {
        let domains = game.geosite.iter().map(|t| format!("geosite:{t}")).collect();
        let ips = game.ips.map(|l| geo.list(l).to_vec()).unwrap_or_default();
        push(domains, ips, warnings);
    }
    out
}

/// Splits user input on newlines, commas and spaces.
fn split_entries(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
        .collect()
}

fn normalize_domain(d: String) -> String {
    const PREFIXES: [&str; 6] = ["domain:", "full:", "keyword:", "regexp:", "geosite:", "ext:"];
    if PREFIXES.iter().any(|p| d.starts_with(p)) {
        d
    } else {
        format!("domain:{}", d.trim_start_matches("*.").trim_start_matches('.'))
    }
}

/// Drops `geoip:`/`geosite:` entries whose tag the geo files lack (xray would
/// not start). `None` when nothing is left.
fn checked(entries: Vec<String>, geo: &GeoData, warnings: &mut Vec<String>) -> Option<Vec<String>> {
    let kept: Vec<String> = entries
        .into_iter()
        .filter(|e| {
            let (set, tag) = if let Some(t) = e.strip_prefix("geoip:") {
                (&geo.geoip, t)
            } else if let Some(t) = e.strip_prefix("geosite:") {
                (&geo.geosite, t)
            } else {
                return true;
            };
            let tag = tag.trim_start_matches('!').split('@').next().unwrap_or_default().to_lowercase();
            let ok = set.contains(&tag);
            if !ok {
                warnings.push(format!("нет базы для {e}"));
            }
            ok
        })
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// Applies the routing settings to a full xray config (before inbounds and
/// interfaces are set by the runtime). Returns warnings for the user.
pub fn apply(config: &mut Value, s: &RoutingSettings, geo: &GeoData) -> Vec<String> {
    let mut warnings = vec![];
    if !config["outbounds"].is_array() {
        return warnings;
    }
    if config["outbounds"][0]["tag"].as_str().unwrap_or_default().is_empty() {
        config["outbounds"][0]["tag"] = json!("proxy");
    }
    let dns_tags: HashSet<String> = config["outbounds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["protocol"] == "dns")
        .filter_map(|o| o["tag"].as_str().map(str::to_string))
        .collect();
    let panel: Vec<Value> = config["routing"]["rules"].as_array().cloned().unwrap_or_default();
    let proxy = proxy_target(config, &panel);

    let ours = user_rules(s, geo, &proxy, &mut warnings);
    let profile = s.advanced.then(|| s.profile()).flatten();
    let catch_all = match profile {
        Some(p) if p.default_action == Action::Direct => Some(Target::Outbound(DIRECT_TAG.into())),
        Some(p) if p.default_action == Action::Block => Some(Target::Outbound(BLOCK_TAG.into())),
        Some(p) if !p.keep_panel_rules => Some(proxy.clone()),
        _ => None,
    };
    let keep_panel = profile.is_none_or(|p| p.keep_panel_rules);

    let (service, rest): (Vec<Value>, Vec<Value>) = panel.into_iter().partition(|r| is_service_rule(r, &dns_tags));
    let mut rules = vec![];
    if s.apps.active() {
        rules.push(json!({ "type": "field", "inboundTag": [DIRECT_INBOUND], "outboundTag": DIRECT_TAG }));
    }
    let has_ip_rules = ours.iter().any(|r| r.get("ip").is_some());
    rules.extend(service);
    rules.extend(ours);
    if let Some(t) = catch_all {
        let mut r = json!({ "type": "field", "network": "tcp,udp" });
        t.apply(&mut r);
        rules.push(r);
    }
    if keep_panel {
        rules.extend(rest);
    }

    let outbounds = config["outbounds"].as_array_mut().unwrap();
    outbounds.retain(|o| o["tag"] != DIRECT_TAG && o["tag"] != BLOCK_TAG);
    outbounds.push(json!({ "tag": DIRECT_TAG, "protocol": "freedom", "settings": { "domainStrategy": "AsIs" } }));
    outbounds.push(json!({ "tag": BLOCK_TAG, "protocol": "blackhole" }));

    if !config["routing"].is_object() {
        config["routing"] = json!({});
    }
    let routing = &mut config["routing"];
    routing["rules"] = json!(rules);
    let strategy = profile.map(|p| p.domain_strategy.as_str()).unwrap_or_default();
    if !strategy.is_empty() {
        routing["domainStrategy"] = json!(strategy);
    } else if has_ip_rules && routing["domainStrategy"].as_str().is_none_or(|d| d == "AsIs") {
        // Sniffed domains replace the destination; resolve them so IP rules match.
        routing["domainStrategy"] = json!("IPIfNonMatch");
    }
    warnings.sort();
    warnings.dedup();
    warnings
}

/// The panel's rules for its own internals (internal DNS inbound, DNS
/// outbound): they must keep matching first.
fn is_service_rule(rule: &Value, dns_tags: &HashSet<String>) -> bool {
    let from_internal = rule["inboundTag"]
        .as_array()
        .is_some_and(|tags| tags.iter().all(|t| t != "socks" && t != "http"));
    let to_dns = rule["outboundTag"].as_str().is_some_and(|t| dns_tags.contains(t));
    from_internal || to_dns
}

/// Where the panel sends traffic by default: its final catch-all rule, else
/// the first outbound (xray's default).
pub fn proxy_target(config: &Value, panel_rules: &[Value]) -> Target {
    const PLAIN: [&str; 5] = ["type", "network", "outboundTag", "balancerTag", "ruleTag"];
    let catch_all = panel_rules.iter().rev().find(|r| {
        r.as_object().is_some_and(|o| o.keys().all(|k| PLAIN.contains(&k.as_str())))
            && r["network"].as_str().is_none_or(|n| n.contains("tcp"))
    });
    if let Some(r) = catch_all {
        if let Some(b) = r["balancerTag"].as_str() {
            return Target::Balancer(b.into());
        }
        if let Some(o) = r["outboundTag"].as_str() {
            return Target::Outbound(o.into());
        }
    }
    Target::Outbound(config["outbounds"][0]["tag"].as_str().unwrap_or("proxy").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo() -> GeoData {
        let mut g = GeoData::load(None, None);
        g.geoip = ["private", "ru", "ru-whitelist"].map(String::from).into();
        g.geosite = ["category-ru", "category-gov-ru", "steam", "riot"].map(String::from).into();
        g
    }

    fn panel() -> Value {
        json!({
            "outbounds": [
                {"tag": "proxy", "protocol": "vless"},
                {"tag": "direct", "protocol": "freedom"},
                {"tag": "dns-out", "protocol": "dns"}
            ],
            "routing": {
                "domainStrategy": "IPIfNonMatch",
                "balancers": [{"tag": "B", "selector": ["proxy"]}],
                "rules": [
                    {"type": "field", "inboundTag": ["dns-internal"], "balancerTag": "B"},
                    {"type": "field", "network": "tcp,udp", "port": "53", "outboundTag": "dns-out"},
                    {"type": "field", "domain": ["domain:vk.com"], "outboundTag": "direct"},
                    {"type": "field", "network": "tcp,udp", "balancerTag": "B"}
                ]
            }
        })
    }

    #[test]
    fn builtin_lists_parse() {
        let g = GeoData::load(None, None);
        assert!(g.list("valve").len() > 20);
        assert!(g.list("valve").iter().all(|c| c.contains('/')));
        assert!(g.list("whitelist-domains").len() > 500);
    }

    #[test]
    fn simple_rules_go_after_service_rules() {
        let mut c = panel();
        let s = RoutingSettings {
            simple: SimpleRouting {
                lan_direct: true,
                ru_direct: true,
                whitelist_direct: false,
                games: vec!["steam".into()],
            },
            ..Default::default()
        };
        let w = apply(&mut c, &s, &geo());
        assert!(w.is_empty(), "{w:?}");
        let rules = c["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules[0]["inboundTag"][0], "dns-internal");
        assert_eq!(rules[1]["outboundTag"], "dns-out");
        assert_eq!(rules[2]["ip"], json!(["geoip:private"]));
        assert_eq!(rules[3]["domain"][0], "geosite:category-ru");
        assert_eq!(rules[4]["ip"], json!(["geoip:ru"]));
        assert!(rules[5]["ip"].as_array().unwrap().len() > 20, "valve IPs");
        assert!(rules[5]["domain"].is_null(), "no Steam store or CDN domains");
        assert!(rules[2..6].iter().all(|r| r["outboundTag"] == DIRECT_TAG));
        assert_eq!(rules[6]["domain"], json!(["domain:vk.com"]), "panel rules kept after ours");
        assert_eq!(rules.last().unwrap()["balancerTag"], "B");
        let tags: Vec<_> = c["outbounds"].as_array().unwrap().iter().map(|o| o["tag"].clone()).collect();
        assert!(tags.contains(&json!(DIRECT_TAG)) && tags.contains(&json!(BLOCK_TAG)));
    }

    #[test]
    fn missing_geo_tags_are_dropped() {
        let mut g = geo();
        g.geoip.remove("ru-whitelist");
        let s = RoutingSettings {
            simple: SimpleRouting { lan_direct: false, whitelist_direct: true, ..Default::default() },
            ..Default::default()
        };
        let mut c = panel();
        let w = apply(&mut c, &s, &g);
        assert_eq!(w, ["нет базы для geoip:ru-whitelist"]);
        let rules = c["routing"]["rules"].as_array().unwrap();
        assert!(rules[2]["domain"].as_array().unwrap().len() > 500, "whitelist domains still used");
        assert_eq!(rules[3]["domain"], json!(["domain:vk.com"]));
    }

    #[test]
    fn advanced_profile() {
        let profile = RouteProfile {
            rules: vec![
                RouteRule {
                    action: Action::Proxy,
                    domains: "youtube.com\n*.googlevideo.com, geosite:nope".into(),
                    ..Default::default()
                },
                RouteRule { action: Action::Block, ips: "1.2.3.0/24".into(), ports: "80, 443".into(), ..Default::default() },
                RouteRule { enabled: false, domains: "x.com".into(), ..Default::default() },
                RouteRule::default(),
            ],
            default_action: Action::Direct,
            keep_panel_rules: false,
            domain_strategy: "AsIs".into(),
            ..Default::default()
        };
        let s = RoutingSettings { advanced: true, profiles: vec![profile], ..Default::default() };
        let mut c = panel();
        let w = apply(&mut c, &s, &geo());
        assert_eq!(w, ["нет базы для geosite:nope"]);
        let rules = c["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 5, "{rules:#?}");
        assert_eq!(rules[2]["domain"], json!(["domain:youtube.com", "domain:googlevideo.com"]));
        assert_eq!(rules[2]["balancerTag"], "B", "proxy = the panel's catch-all target");
        assert_eq!(rules[3]["outboundTag"], BLOCK_TAG);
        assert_eq!(rules[3]["port"], "80,443");
        assert_eq!(rules[4]["outboundTag"], DIRECT_TAG, "default: direct");
        assert_eq!(c["routing"]["domainStrategy"], "AsIs");
    }

    #[test]
    fn apps_rule_first_and_link_configs() {
        let mut c = json!({ "outbounds": [{"protocol": "vless"}] });
        let s = RoutingSettings {
            apps: AppRouting { mode: AppMode::Bypass, apps: vec!["steam.exe".into()] },
            simple: SimpleRouting { lan_direct: true, ..Default::default() },
            ..Default::default()
        };
        apply(&mut c, &s, &geo());
        assert_eq!(c["outbounds"][0]["tag"], "proxy");
        assert_eq!(c["routing"]["rules"][0]["inboundTag"], json!([DIRECT_INBOUND]));
        assert_eq!(c["routing"]["domainStrategy"], "IPIfNonMatch");
    }

    #[test]
    fn reads_dat_tags() {
        // Two entries: tag "RU" and tag "private", each followed by junk fields.
        let entry = |tag: &str| {
            let mut e = vec![0x0a, tag.len() as u8];
            e.extend(tag.as_bytes());
            e.extend([0x12, 0x02, 0xff, 0xff]);
            let mut out = vec![0x0a, e.len() as u8];
            out.extend(e);
            out
        };
        let mut data = entry("RU");
        data.extend(entry("private"));
        assert_eq!(dat_tags(&data), ["ru", "private"].map(String::from).into());
    }

    fn put_varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
    }

    fn bytes_field(number: u32, value: &[u8], out: &mut Vec<u8>) {
        put_varint(u64::from(number) << 3 | 2, out);
        put_varint(value.len() as u64, out);
        out.extend(value);
    }

    fn varint_field(number: u32, value: u64, out: &mut Vec<u8>) {
        put_varint(u64::from(number) << 3, out);
        put_varint(value, out);
    }

    #[test]
    fn searches_geosite_domains_and_tags() {
        let mut domain = Vec::new();
        varint_field(1, 3, &mut domain);
        bytes_field(2, "пример.рф".as_bytes(), &mut domain);
        let mut entry = Vec::new();
        bytes_field(1, b"Category-RU", &mut entry);
        bytes_field(2, &domain, &mut entry);
        let mut data = Vec::new();
        bytes_field(1, &entry, &mut data);

        assert_eq!(
            search_geosite(&data, "ПРИМЕР", 10),
            [GeoMatch { tag: "Category-RU".into(), value: "full:пример.рф".into() }]
        );
        assert_eq!(search_geosite(&data, "category-ru", 10).len(), 1);
        assert!(search_geosite(&data, "missing", 10).is_empty());
        assert!(search_geosite(&data, "", 10).is_empty());
    }

    #[test]
    fn searches_geoip_ipv4_and_ipv6() {
        let mut data = Vec::new();
        for (tag, ip, prefix) in [
            ("ru", std::net::IpAddr::from([1, 2, 3, 0]), 24),
            ("cloud", "2001:db8::".parse().unwrap(), 32),
        ] {
            let mut cidr = Vec::new();
            match ip {
                std::net::IpAddr::V4(ip) => bytes_field(1, &ip.octets(), &mut cidr),
                std::net::IpAddr::V6(ip) => bytes_field(1, &ip.octets(), &mut cidr),
            }
            varint_field(2, prefix, &mut cidr);
            let mut entry = Vec::new();
            bytes_field(1, tag.as_bytes(), &mut entry);
            bytes_field(2, &cidr, &mut entry);
            bytes_field(1, &entry, &mut data);
        }

        assert_eq!(search_geoip(&data, "1.2.3.0/24", 10)[0].value, "1.2.3.0/24");
        assert_eq!(search_geoip(&data, "2001:DB8", 10)[0].tag, "cloud");
        assert_eq!(search_geoip(&data, "cloud", 1).len(), 1);
        assert_eq!(search_geoip(&data, "1.2.3.77", 10)[0].tag, "ru", "an address finds its network");
        assert_eq!(search_geoip(&data, "2001:db8:ffff::1", 10)[0].tag, "cloud");
        assert!(search_geoip(&data, "1.2.4.1", 10).is_empty());
    }

    #[test]
    fn geo_query_prefix_names_the_file() {
        assert_eq!(split_geo_query(" GeoIP:ru "), (Some("geoip"), "ru"));
        assert_eq!(split_geo_query("geosite:google"), (Some("geosite"), "google"));
        assert_eq!(split_geo_query("ru"), (None, "ru"));
        assert!(cidr_contains("0.0.0.0/0", "8.8.8.8".parse().unwrap()));
        assert!(!cidr_contains("10.0.0.0/8", "::1".parse().unwrap()));
    }

    #[test]
    fn malformed_dat_is_ignored() {
        for data in [&[0x0a, 0x80][..], &[0x0a, 5, 0x0a, 1, b'x'][..], &[0x0b, 1, 0][..]] {
            assert!(dat_tags(data).is_empty());
            assert!(search_geosite(data, "x", 10).is_empty());
            assert!(search_geoip(data, "x", 10).is_empty());
        }
    }
}

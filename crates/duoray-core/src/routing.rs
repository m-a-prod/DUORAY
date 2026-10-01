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
        subtitle: "Магазин, загрузки, матчи CS2 и Dota 2",
        geosite: &["steam"],
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

/// Lower-cased tags of a v2ray `geoip.dat`/`geosite.dat` (a protobuf list of
/// entries whose first field is the tag). Reads only the framing.
pub fn dat_tags(data: &[u8]) -> HashSet<String> {
    fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let x = *b.get(*i)?;
            *i += 1;
            v |= u64::from(x & 0x7f) << shift;
            if x < 0x80 {
                return Some(v);
            }
        }
        None
    }
    let mut tags = HashSet::new();
    let mut i = 0;
    while i < data.len() {
        let (Some(_key), Some(len)) = (varint(data, &mut i), varint(data, &mut i)) else { break };
        let end = i.saturating_add(len as usize).min(data.len());
        let entry = &data[i..end];
        i = end;
        let mut j = 0;
        if let (Some(_), Some(n)) = (varint(entry, &mut j), varint(entry, &mut j))
            && let Some(tag) = entry.get(j..j + n as usize)
        {
            tags.insert(String::from_utf8_lossy(tag).to_lowercase());
        }
    }
    tags
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
        assert_eq!(rules[5]["domain"], json!(["geosite:steam"]));
        assert!(rules[6]["ip"].as_array().unwrap().len() > 20, "valve IPs");
        assert!(rules[2..7].iter().all(|r| r["outboundTag"] == DIRECT_TAG));
        assert_eq!(rules[7]["domain"], json!(["domain:vk.com"]), "panel rules kept after ours");
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
}

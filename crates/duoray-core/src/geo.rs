//! Geo databases for routing, kept in `<data>/geo`:
//! - `geoip.dat` from runetfreedom (has `ru`, `ru-whitelist`, `private`, ...),
//!   refreshed daily;
//! - `geosite.dat` copied from the one shipped with xray (domain-list-community);
//! - `lists/*.txt`: game server IPs (Steam relays, Valve/Riot/Blizzard ASNs)
//!   and the mobile whitelist domains, refreshed weekly.
//!
//! Every download is validated before it replaces the old file, so a broken
//! update never stops xray from starting.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::routing::dat_tags;

const GEOIP_URL: &str = "https://raw.githubusercontent.com/runetfreedom/russia-v2ray-rules-dat/release/geoip.dat";
const WHITELIST_URL: &str =
    "https://raw.githubusercontent.com/hxehex/russia-mobile-internet-whitelist/main/whitelist.txt";
type ListFetch = fn() -> Result<Vec<String>>;

const DAY: Duration = Duration::from_secs(24 * 3600);
const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

pub struct GeoDir {
    pub dir: PathBuf,
}

impl GeoDir {
    pub fn new(data_dir: &Path) -> Self {
        Self { dir: data_dir.join("geo") }
    }

    pub fn lists(&self) -> PathBuf {
        self.dir.join("lists")
    }

    /// The directory to hand xray as `XRAY_LOCATION_ASSET`, once both files exist.
    pub fn assets(&self) -> Option<&Path> {
        (self.dir.join("geoip.dat").is_file() && self.dir.join("geosite.dat").is_file()).then_some(&self.dir)
    }

    /// When `geoip.dat` was last updated.
    pub fn updated(&self) -> Option<SystemTime> {
        std::fs::metadata(self.dir.join("geoip.dat")).and_then(|m| m.modified()).ok()
    }

    /// Brings everything up to date. `bundled` is the directory with the
    /// geo files shipped with xray. With `force`, ignores the age of files.
    pub fn update(&self, bundled: Option<&Path>, force: bool) -> Result<()> {
        std::fs::create_dir_all(self.lists())?;
        let mut errors = vec![];

        if let Some(b) = bundled {
            let (src, dst) = (b.join("geosite.dat"), self.dir.join("geosite.dat"));
            let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).ok();
            if src.is_file() && size(&src) != size(&dst) {
                let tmp = dst.with_extension("tmp");
                std::fs::copy(&src, &tmp)?;
                std::fs::rename(&tmp, &dst)?;
            }
        }

        if force || stale(&self.dir.join("geoip.dat"), DAY) {
            let r = download(GEOIP_URL, Duration::from_secs(120)).and_then(|data| {
                let tags = dat_tags(&data);
                if !["ru", "private"].iter().all(|t| tags.contains(*t)) {
                    bail!("geoip.dat без нужных разделов");
                }
                replace(&self.dir.join("geoip.dat"), &data)
            });
            if let Err(e) = r {
                errors.push(format!("geoip: {e:#}"));
            }
        }

        let lists: [(&str, ListFetch); 4] = [
            ("valve", valve_ips),
            ("riot", || asn_prefixes(6507)),
            ("blizzard", || asn_prefixes(57976)),
            ("whitelist-domains", whitelist_domains),
        ];
        for (name, fetch) in lists {
            let path = self.lists().join(format!("{name}.txt"));
            if !force && !stale(&path, WEEK) {
                continue;
            }
            match fetch() {
                Ok(items) if !items.is_empty() => {
                    let mut text = format!("# {name}: updated by DUORAY\n");
                    for i in items {
                        text.push_str(&i);
                        text.push('\n');
                    }
                    replace(&path, text.as_bytes())?;
                }
                Ok(_) => errors.push(format!("{name}: пустой список")),
                Err(e) => errors.push(format!("{name}: {e:#}")),
            }
        }
        if errors.is_empty() { Ok(()) } else { bail!(errors.join("; ")) }
    }
}

fn stale(path: &Path, max_age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age > max_age)
}

fn replace(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(concat!("Duoray/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn download(url: &str, timeout: Duration) -> Result<Vec<u8>> {
    let mut resp = agent(timeout).get(url).call().with_context(|| url.to_string())?;
    Ok(resp.body_mut().with_config().limit(64 << 20).read_to_vec()?)
}

fn json(url: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice(&download(url, Duration::from_secs(30))?)?)
}

/// Prefixes announced by an autonomous system (RIPEstat).
fn asn_prefixes(asn: u32) -> Result<Vec<String>> {
    let v = json(&format!("https://stat.ripe.net/data/announced-prefixes/data.json?resource=AS{asn}"))?;
    let set: BTreeSet<String> = v["data"]["prefixes"]
        .as_array()
        .context("RIPEstat: нет prefixes")?
        .iter()
        .filter_map(|p| p["prefix"].as_str().map(str::to_string))
        .collect();
    Ok(set.into_iter().collect())
}

/// Steam Datagram Relay addresses (CS2 and Dota 2 matchmaking goes through
/// them) plus everything Valve announces.
fn valve_ips() -> Result<Vec<String>> {
    let mut set: BTreeSet<String> = asn_prefixes(32590)?.into_iter().collect();
    for app in [730, 570] {
        let v = json(&format!("https://api.steampowered.com/ISteamApps/GetSDRConfig/v1/?appid={app}"))?;
        for pop in v["pops"].as_object().into_iter().flat_map(|o| o.values()) {
            for relay in pop["relays"].as_array().into_iter().flatten() {
                if let Some(ip) = relay["ipv4"].as_str() {
                    set.insert(format!("{ip}/32"));
                }
            }
        }
    }
    Ok(set.into_iter().collect())
}

fn whitelist_domains() -> Result<Vec<String>> {
    let text = String::from_utf8(download(WHITELIST_URL, Duration::from_secs(30))?)?;
    let set: BTreeSet<String> = crate::routing::parse_list(&text)
        .into_iter()
        .map(|d| d.to_lowercase())
        .filter(|d| d.contains('.') && d.chars().all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c)))
        .collect();
    Ok(set.into_iter().collect())
}

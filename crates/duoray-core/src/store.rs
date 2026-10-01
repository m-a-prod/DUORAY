//! Persistent app state: subscriptions and settings, one JSON file.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity::HappSpoof;
use crate::server::Server;
use crate::subscription::{Fetched, SubInfo};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub subscriptions: Vec<Subscription>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub happ: HappSpoof,
    pub ping: PingSettings,
    /// Last selected server (GUI selection key), restored on start.
    pub last_server: Option<String>,
    /// Send x-hwid and device headers with subscription requests. Panels with
    /// device limits need them; off means only the User-Agent is sent.
    pub send_device_info: bool,
    /// Text size in percent (100 = default).
    pub text_scale: u32,
    /// Font family; empty means the system font.
    pub font: String,
    /// "dark" (default), "light" or "system".
    pub theme: String,
    pub routing: crate::routing::RoutingSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            happ: HappSpoof::default(),
            ping: PingSettings::default(),
            last_server: None,
            send_device_info: true,
            text_scale: 100,
            font: String::new(),
            theme: "dark".into(),
            routing: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PingMode {
    /// HTTP GET through the server itself (real delay; needs xray).
    #[default]
    #[serde(alias = "http")]
    HttpGet,
    /// Like GET, without a response body.
    HttpHead,
    /// TCP connect time to the server.
    Tcp,
    /// System `ping`.
    Icmp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PingSettings {
    pub mode: PingMode,
    /// Servers pinged in parallel. 1 is gentle; many at once can trip
    /// providers that whitelist traffic.
    pub threads: u32,
    /// Target of the HTTP mode.
    pub url: String,
    pub timeout_ms: u64,
}

impl Default for PingSettings {
    fn default() -> Self {
        Self {
            mode: PingMode::HttpGet,
            threads: 1,
            url: "http://cp.cloudflare.com/generate_204".into(),
            timeout_ms: 5000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Subscription {
    pub id: String,
    /// User-given name; empty means "use the panel's title".
    pub name: String,
    pub url: String,
    pub updated_at: Option<u64>,
    pub info: SubInfo,
    pub servers: Vec<Server>,
    pub skipped: usize,
    pub json: bool,
    pub last_error: Option<String>,
    /// The user collapsed this subscription's announcement.
    pub announce_hidden: bool,
}

/// Name of the group that holds servers added as plain share links.
pub const MANUAL_GROUP_NAME: &str = "Мои серверы";

/// True if the text holds share links (vless://, ss://, …) rather than a subscription URL.
pub fn looks_like_links(text: &str) -> bool {
    text.lines().map(str::trim).any(|l| {
        l.contains("://") && !l.starts_with("http://") && !l.starts_with("https://")
    })
}

impl Subscription {
    /// Servers added by hand as share links, not fetched from a URL.
    pub fn is_manual(&self) -> bool {
        self.url.is_empty()
    }

    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        if let Some(t) = &self.info.title {
            return t.clone();
        }
        url::Url::parse(&self.url)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_else(|| self.url.clone())
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Store {
    /// `DUORAY_STORE` overrides the location (handy for testing).
    pub fn default_path() -> Result<PathBuf> {
        if let Some(p) = std::env::var_os("DUORAY_STORE") {
            return Ok(PathBuf::from(p));
        }
        let dirs = directories::ProjectDirs::from("space", "dualizm", "Duoray")
            .context("cannot determine the app data directory")?;
        Ok(dirs.data_dir().join("store.json"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Atomic: write a temp file, then rename over the old one.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn add(&mut self, url: &str, name: &str) -> Result<String> {
        let url = url.trim();
        let parsed = url::Url::parse(url).context("not a valid URL")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("subscription URL must be http(s)");
        }
        if self.subscriptions.iter().any(|s| s.url == url) {
            anyhow::bail!("this subscription is already added");
        }
        let id = format!("{:x}{:04x}", now(), fastrand::u16(..));
        self.subscriptions.push(Subscription {
            id: id.clone(),
            name: name.trim().to_string(),
            url: url.to_string(),
            ..Default::default()
        });
        Ok(id)
    }

    /// Adds share links (one or many lines) to the "Мои серверы" group,
    /// creating it if needed. Returns the group id and how many were new.
    pub fn add_links(&mut self, text: &str) -> Result<(String, usize)> {
        let mut profiles = vec![];
        let mut errors = vec![];
        for line in text.lines().map(str::trim).filter(|l| l.contains("://")) {
            match crate::link::parse(line) {
                Ok(p) => profiles.push(p),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if profiles.is_empty() {
            anyhow::bail!(errors.into_iter().next().unwrap_or_else(|| "no share links found".into()));
        }
        let index = match self.subscriptions.iter().position(Subscription::is_manual) {
            Some(i) => i,
            None => {
                self.subscriptions.push(Subscription {
                    id: format!("{:x}{:04x}", now(), fastrand::u16(..)),
                    name: MANUAL_GROUP_NAME.into(),
                    updated_at: Some(now()),
                    ..Default::default()
                });
                self.subscriptions.len() - 1
            }
        };
        let group = &mut self.subscriptions[index];
        let mut added = 0;
        for p in profiles {
            let known = group.servers.iter().any(|s| matches!(&s.source, crate::server::Source::Link(q) if q.link == p.link));
            if !known {
                group.servers.push(Server::from_link(p));
                added += 1;
            }
        }
        group.updated_at = Some(now());
        Ok((group.id.clone(), added))
    }

    pub fn get(&self, id: &str) -> Option<&Subscription> {
        self.subscriptions.iter().find(|s| s.id == id)
    }

    pub fn apply(&mut self, id: &str, result: Result<Fetched, String>) {
        let Some(sub) = self.subscriptions.iter_mut().find(|s| s.id == id) else { return };
        match result {
            Ok(f) => {
                // The panel moved: follow it permanently.
                if let Some(new) = crate::subscription::replacement_url(&f.info, &sub.url) {
                    sub.url = new;
                }
                sub.info = f.info;
                sub.servers = f.servers;
                sub.skipped = f.skipped.len();
                sub.json = f.json;
                sub.updated_at = Some(now());
                sub.last_error = None;
            }
            // Keep the last good server list.
            Err(e) => sub.last_error = Some(e),
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.subscriptions.retain(|s| s.id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_links() {
        let mut s = Store::default();
        let text = "vless://id@1.2.3.4:443?security=tls#A\ntrojan://pw@t.example:443#B\n";
        assert!(looks_like_links(text));
        assert!(!looks_like_links("https://p.example/sub/x"));
        let (id, added) = s.add_links(text).unwrap();
        assert_eq!(added, 2);
        let (id2, added2) = s.add_links("vless://id@1.2.3.4:443?security=tls#A").unwrap();
        assert_eq!((id2 == id, added2), (true, 0), "same group, duplicate skipped");
        assert!(s.get(&id).unwrap().is_manual());
        assert_eq!(s.get(&id).unwrap().display_name(), MANUAL_GROUP_NAME);
        assert!(s.add_links("garbage://x").is_err());
    }

    #[test]
    fn roundtrip_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");
        let mut s = Store::default();
        let id = s.add("https://p.example/sub/x", "").unwrap();
        assert!(s.add("https://p.example/sub/x", "").is_err(), "duplicate");
        assert!(s.add("ftp://x", "").is_err());
        assert!(s.add("not a url", "").is_err());
        s.apply(&id, Err("boom".into()));
        s.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.subscriptions.len(), 1);
        assert_eq!(loaded.subscriptions[0].last_error.as_deref(), Some("boom"));
        assert_eq!(loaded.subscriptions[0].display_name(), "p.example");
    }
}

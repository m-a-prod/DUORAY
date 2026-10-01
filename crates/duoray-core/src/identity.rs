//! Who we say we are when fetching subscriptions.
//!
//! By default Duoray identifies as itself (plus `x-hwid` & co. so panels with
//! per-device limits work). "Happ Spoof" reproduces the Happ client's request
//! fingerprint for panels that only serve Happ; format follows BetterExclave.

use serde::{Deserialize, Serialize};

use crate::device::Device;

pub const DUORAY_USER_AGENT: &str = concat!("Duoray/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HappSpoof {
    pub enabled: bool,
    pub app_version: String,
    /// Android | iOS | macOS | Windows
    pub os: String,
    pub os_version: String,
    pub model: String,
    pub locale: String,
    /// Trailing 20-digit number of the Happ User-Agent.
    pub user_id: String,
    /// `x-hwid`: 16 hex digits.
    pub hwid: String,
}

impl Default for HappSpoof {
    fn default() -> Self {
        let device = Device::detect();
        Self {
            enabled: false,
            app_version: "3.23.0".into(),
            os: device.os.to_string(),
            os_version: device.os_version.unwrap_or_default(),
            model: device.model.unwrap_or_default(),
            locale: system_locale(),
            user_id: random_user_id(),
            hwid: random_hwid(),
        }
    }
}

impl HappSpoof {
    pub fn user_agent(&self) -> String {
        format!("Happ/{}/{}/{}", self.app_version, self.os, self.user_id)
    }

    pub fn randomize_ids(&mut self) {
        self.user_id = random_user_id();
        self.hwid = random_hwid();
    }

    /// Coherent random platform/version/model plus fresh IDs.
    pub fn randomize_device(&mut self) {
        let (os, versions, models) = DEVICES[fastrand::usize(..DEVICES.len())];
        self.os = os.into();
        self.os_version = versions[fastrand::usize(..versions.len())].into();
        self.model = models[fastrand::usize(..models.len())].into();
        self.app_version = APP_VERSIONS[fastrand::usize(..APP_VERSIONS.len())].into();
        self.locale = LOCALES[fastrand::usize(..LOCALES.len())].into();
        self.randomize_ids();
    }
}

const APP_VERSIONS: &[&str] = &["3.23.0", "3.22.1", "3.21.1", "3.21.0", "3.20.4"];
const LOCALES: &[&str] = &["ru", "en", "uk", "tr", "de", "fa"];
const DEVICES: &[(&str, &[&str], &[&str])] = &[
    ("macOS", &["14.7", "15.5", "15.6", "26.0", "26.3"], &["Mac14,2", "Mac15,3", "Mac15,6", "Mac16,1", "Mac16,8"]),
    ("Windows", &["10.0.19045", "10.0.22631", "10.0.26100"], &["Desktop PC", "Laptop"]),
    ("iOS", &["17.6", "18.3", "18.6", "26.0"], &["iPhone15,3", "iPhone16,1", "iPhone17,1", "iPhone17,3"]),
    ("Android", &["13", "14", "15", "16"], &["Pixel 8", "Pixel 9 Pro", "SM-S921B", "SM-S928B", "Redmi Note 13 Pro"]),
];

fn random_user_id() -> String {
    let mut s = fastrand::u8(1..=9).to_string();
    for _ in 0..19 {
        s.push(char::from(b'0' + fastrand::u8(0..10)));
    }
    s
}

fn random_hwid() -> String {
    format!("{:016x}", fastrand::u64(..))
}

fn system_locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|l| l.get(..2).map(str::to_lowercase))
        .filter(|l| l.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or_else(|| "ru".into())
}

/// Request headers for a subscription fetch.
/// `send_device_info: false` sends only the User-Agent (no HWID, OS, model).
pub fn request_headers(happ: &HappSpoof, device: &Device, send_device_info: bool) -> Vec<(String, String)> {
    let mut h: Vec<(&str, String)> = if happ.enabled {
        vec![
            ("User-Agent", happ.user_agent()),
            ("x-hwid", happ.hwid.clone()),
            ("x-device-os", happ.os.clone()),
            ("x-ver-os", happ.os_version.clone()),
            ("x-device-model", happ.model.clone()),
            ("x-device-locale", happ.locale.clone()),
        ]
    } else {
        vec![
            ("User-Agent", DUORAY_USER_AGENT.to_string()),
            ("x-hwid", device.hwid.clone()),
            ("x-device-os", device.os.to_string()),
            ("x-ver-os", device.os_version.clone().unwrap_or_default()),
            ("x-device-model", device.model.clone().unwrap_or_default()),
        ]
    };
    h.retain(|(k, v)| !v.is_empty() && (send_device_info || *k == "User-Agent"));
    h.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happ_user_agent_format() {
        let mut h = HappSpoof::default();
        h.randomize_device();
        let ua = h.user_agent();
        let parts: Vec<&str> = ua.split('/').collect();
        assert_eq!(parts.len(), 4, "{ua}");
        assert_eq!(parts[0], "Happ");
        assert_eq!(parts[3].len(), 20);
        assert_eq!(h.hwid.len(), 16);
    }

    #[test]
    fn default_identity_is_duoray() {
        let h = HappSpoof::default();
        let hdrs = request_headers(&h, &Device::detect(), true);
        assert_eq!(hdrs[0], ("User-Agent".to_string(), DUORAY_USER_AGENT.to_string()));
        assert!(hdrs.iter().any(|(k, _)| k == "x-hwid"));
        let only_ua = request_headers(&h, &Device::detect(), false);
        assert_eq!(only_ua.len(), 1);
        assert_eq!(only_ua[0].0, "User-Agent");
    }
}

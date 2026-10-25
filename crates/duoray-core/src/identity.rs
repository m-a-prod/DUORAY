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

/// How one subscription identifies itself, over the app-wide settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubIdentity {
    pub mode: SubIdentityMode,
    /// DUORAY mode: `x-hwid` and User-Agent; empty keeps the device's / ours.
    pub hwid: String,
    pub user_agent: String,
    /// Happ mode: this subscription's own Happ fingerprint.
    pub happ: Option<HappSpoof>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubIdentityMode {
    /// As in Settings (DUORAY or the app-wide Happ Spoof).
    #[default]
    Settings,
    Duoray,
    Happ,
}

/// Request headers for one subscription. Its own identity, set by hand for
/// exactly this panel, is sent whole: device info "off" in Settings does
/// not strip a HWID the user typed or a Happ fingerprint (Happ sends one).
pub fn subscription_headers(global: &HappSpoof, device: &Device, send_device_info: bool, own: &SubIdentity) -> Vec<(String, String)> {
    match own.mode {
        SubIdentityMode::Settings => request_headers(global, device, send_device_info),
        SubIdentityMode::Duoray => {
            let duoray = HappSpoof { enabled: false, ..global.clone() };
            with_custom(request_headers(&duoray, device, send_device_info), &own.hwid, &own.user_agent)
        }
        SubIdentityMode::Happ => {
            let happ = HappSpoof { enabled: true, ..own.happ.clone().unwrap_or_else(|| global.clone()) };
            request_headers(&happ, device, true)
        }
    }
}

/// A random HWID in the device format (UUID-shaped), for DUORAY mode.
pub fn random_device_hwid() -> String {
    let (a, b) = (fastrand::u64(..), fastrand::u64(..));
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xffff, a & 0xffff, b >> 48, b & 0xffff_ffff_ffff)
}

/// A subscription's own HWID and User-Agent over [`request_headers`]; empty
/// keeps the app-wide value. A HWID set by hand is sent even with device
/// info off: the user asked for exactly that one.
fn with_custom(mut headers: Vec<(String, String)>, hwid: &str, user_agent: &str) -> Vec<(String, String)> {
    for (key, value) in [("User-Agent", user_agent.trim()), ("x-hwid", hwid.trim())] {
        if value.is_empty() {
            continue;
        }
        match headers.iter_mut().find(|(k, _)| k == key) {
            Some(h) => h.1 = value.to_string(),
            None => headers.push((key.to_string(), value.to_string())),
        }
    }
    headers
}

/// Why a hand-typed header value cannot be sent, if it cannot: HTTP header
/// values are one line of printable ASCII.
pub fn header_value_problem(value: &str) -> Option<&'static str> {
    let value = value.trim();
    if value.len() > 512 {
        Some("слишком длинное значение (больше 512 символов)")
    } else if !value.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        Some("только латиница, цифры и знаки, без переносов строк")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_identity_modes() {
        let device = Device::detect();
        let global = HappSpoof { enabled: true, ..HappSpoof::default() };
        let ua = |h: &[(String, String)]| h.iter().find(|(k, _)| k == "User-Agent").unwrap().1.clone();
        let hwid = |h: &[(String, String)]| h.iter().find(|(k, _)| k == "x-hwid").map(|(_, v)| v.clone());

        let own = SubIdentity::default();
        assert_eq!(subscription_headers(&global, &device, true, &own), request_headers(&global, &device, true));

        let own = SubIdentity { mode: SubIdentityMode::Duoray, hwid: "H-1".into(), ..Default::default() };
        let h = subscription_headers(&global, &device, false, &own);
        assert_eq!(ua(&h), DUORAY_USER_AGENT, "DUORAY mode drops the app-wide Happ");
        assert_eq!(hwid(&h).as_deref(), Some("H-1"));

        let mine = HappSpoof { os: "iOS".into(), user_id: "12345678901234567890".into(), hwid: "abcd".into(), ..HappSpoof::default() };
        let own = SubIdentity { mode: SubIdentityMode::Happ, happ: Some(mine), ..Default::default() };
        let h = subscription_headers(&HappSpoof::default(), &device, false, &own);
        assert!(ua(&h).starts_with("Happ/") && ua(&h).ends_with("/iOS/12345678901234567890"), "{}", ua(&h));
        assert_eq!(hwid(&h).as_deref(), Some("abcd"), "Happ fingerprint sent whole");
        assert_eq!(random_device_hwid().len(), 36);
    }

    #[test]
    fn custom_hwid_and_user_agent() {
        let base = request_headers(&HappSpoof::default(), &Device::detect(), false);
        assert_eq!(with_custom(base.clone(), "", " "), base, "empty keeps the defaults");
        let h = with_custom(base, " ABC-1 ", "MyClient/2");
        assert_eq!(h[0], ("User-Agent".to_string(), "MyClient/2".to_string()));
        assert!(h.contains(&("x-hwid".to_string(), "ABC-1".to_string())), "sent even with device info off");
        assert_eq!(header_value_problem("Happ/3.1 (iPhone)"), None);
        assert!(header_value_problem("a\nb").is_some());
        assert!(header_value_problem("хвид").is_some());
    }

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

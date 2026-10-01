//! Device identity sent with subscription requests (`x-hwid` & co.), which
//! panels such as Remnawave use for per-user device limits.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::process::Command;

pub struct Device {
    pub hwid: String,
    pub os: &'static str,
    pub os_version: Option<String>,
    pub model: Option<String>,
}

impl Device {
    pub fn detect() -> Self {
        Self {
            hwid: hwid(),
            os: os_name(),
            os_version: os_version(),
            model: model(),
        }
    }
}

fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(windows) {
        "Windows"
    } else {
        "Linux"
    }
}

fn cmd(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        // The GUI has no console; don't flash one for these probes.
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let out = command.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Stable per machine, but derived: the raw machine id never leaves the device.
fn hwid() -> String {
    let raw = machine_id().unwrap_or_else(|| {
        // No machine id available: fall back to something stable-ish for this user.
        format!("{}-{}", std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default(), os_name())
    });
    let half = |salt: &str| {
        let mut h = DefaultHasher::new();
        ("duoray-hwid", salt, &raw).hash(&mut h);
        h.finish()
    };
    let (a, b) = (half("a"), half("b"));
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0xffff,
        b >> 48,
        b & 0xffff_ffff_ffff
    )
}

fn machine_id() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = cmd("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])?;
        return out
            .lines()
            .find(|l| l.contains("IOPlatformUUID"))
            .and_then(|l| l.split('"').nth(3))
            .map(String::from);
    }
    #[cfg(target_os = "linux")]
    {
        return std::fs::read_to_string("/etc/machine-id")
            .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    #[cfg(windows)]
    {
        let out = cmd(
            "reg",
            &["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"],
        )?;
        return out
            .lines()
            .find(|l| l.contains("MachineGuid"))
            .and_then(|l| l.split_whitespace().last())
            .map(String::from);
    }
    #[allow(unreachable_code)]
    None
}

fn os_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    return cmd("/usr/bin/sw_vers", &["-productVersion"]);
    #[cfg(target_os = "linux")]
    return std::fs::read_to_string("/etc/os-release").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("VERSION_ID="))
            .map(|v| v.trim_matches('"').to_string())
    });
    #[cfg(windows)]
    return cmd("cmd", &["/C", "ver"]);
    #[allow(unreachable_code)]
    None
}

fn model() -> Option<String> {
    #[cfg(target_os = "macos")]
    return cmd("/usr/sbin/sysctl", &["-n", "hw.model"]);
    #[cfg(target_os = "linux")]
    return std::fs::read_to_string("/sys/class/dmi/id/product_name")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    #[allow(unreachable_code)]
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn hwid_is_stable_and_uuid_shaped() {
        let a = super::hwid();
        assert_eq!(a, super::hwid());
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
    }
}

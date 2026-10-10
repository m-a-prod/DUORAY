//! System proxy for proxy mode on Windows and Linux (macOS: the helper sets
//! it, see duoray-helper's macproxy). The settings found before are written to
//! `run/sysproxy.json` first, so they come back on disconnect, and also on the
//! next start if DUORAY died while connected (the proxy would point at a dead
//! port and cut the internet otherwise).

use std::path::{Path, PathBuf};

use anyhow::Result;

const STATE: &str = "sysproxy.json";

/// Puts the previous settings back when dropped.
pub struct Guard {
    path: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore_file(&self.path);
    }
}

/// Points the system proxy at `127.0.0.1:port`. The warning says when this
/// desktop has no system proxy settings we know (apps then need it by hand).
pub fn enable(run_dir: &Path, port: u16) -> Result<(Guard, Option<String>)> {
    let path = run_dir.join(STATE);
    restore_file(&path);
    std::fs::create_dir_all(run_dir)?;
    let warning = imp::enable(port, |saved| crate::connection::write_private(&path, &serde_json::to_vec(saved)?))?;
    Ok((Guard { path }, warning))
}

/// After a crash: restores what an unfinished session saved.
pub fn recover(run_dir: &Path) {
    restore_file(&run_dir.join(STATE));
}

fn restore_file(path: &Path) {
    let Ok(data) = std::fs::read(path) else { return };
    match serde_json::from_slice::<imp::Saved>(&data) {
        Ok(saved) => {
            imp::restore(&saved);
            crate::diag::log("system proxy restored");
        }
        Err(e) => crate::diag::log(format!("system proxy: unreadable {}: {e}", path.display())),
    }
    let _ = std::fs::remove_file(path);
}

#[cfg(windows)]
mod imp {
    //! WinINet's LAN settings (what "Settings → Proxy" shows; browsers and
    //! most apps follow them), changed through the API so running apps are
    //! told at once.

    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use anyhow::{Result, bail};
    use serde::{Deserialize, Serialize};
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Networking::WinInet::{
        INTERNET_OPTION_PER_CONNECTION_OPTION, INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED,
        INTERNET_PER_CONN_AUTOCONFIG_URL, INTERNET_PER_CONN_FLAGS, INTERNET_PER_CONN_OPTION_LISTW,
        INTERNET_PER_CONN_OPTIONW, INTERNET_PER_CONN_PROXY_BYPASS, INTERNET_PER_CONN_PROXY_SERVER, InternetQueryOptionW,
        InternetSetOptionW, PROXY_TYPE_DIRECT, PROXY_TYPE_PROXY,
    };

    /// Local and private addresses stay direct.
    const BYPASS: &str = "localhost;127.*;10.*;192.168.*;169.254.*;172.16.*;172.17.*;172.18.*;172.19.*;172.20.*;\
                          172.21.*;172.22.*;172.23.*;172.24.*;172.25.*;172.26.*;172.27.*;172.28.*;172.29.*;\
                          172.30.*;172.31.*;<local>";

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Saved {
        flags: u32,
        server: Option<String>,
        bypass: Option<String>,
        pac: Option<String>,
    }

    pub fn enable(port: u16, save: impl FnOnce(&Saved) -> Result<()>) -> Result<Option<String>> {
        let before = read()?;
        save(&before)?;
        write(&Saved {
            // Without auto-detect and the setup script: they would win.
            flags: PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY,
            // One address for every scheme: an HTTP proxy (WinINet's
            // "socks=" would mean SOCKS4).
            server: Some(format!("127.0.0.1:{port}")),
            bypass: Some(BYPASS.into()),
            pac: before.pac.clone(),
        })?;
        Ok(None)
    }

    pub fn restore(saved: &Saved) {
        if let Err(e) = write(saved) {
            crate::diag::log(format!("system proxy: restore failed: {e:#}"));
        }
    }

    fn options() -> [INTERNET_PER_CONN_OPTIONW; 4] {
        [INTERNET_PER_CONN_FLAGS, INTERNET_PER_CONN_PROXY_SERVER, INTERNET_PER_CONN_PROXY_BYPASS, INTERNET_PER_CONN_AUTOCONFIG_URL]
            .map(|option| INTERNET_PER_CONN_OPTIONW { dwOption: option, ..Default::default() })
    }

    fn list(options: &mut [INTERNET_PER_CONN_OPTIONW; 4]) -> INTERNET_PER_CONN_OPTION_LISTW {
        INTERNET_PER_CONN_OPTION_LISTW {
            dwSize: size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
            // The LAN settings (not a dial-up connection's).
            pszConnection: null_mut(),
            dwOptionCount: options.len() as u32,
            dwOptionError: 0,
            pOptions: options.as_mut_ptr(),
        }
    }

    fn read() -> Result<Saved> {
        let mut opts = options();
        let mut l = list(&mut opts);
        let mut len = size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32;
        // SAFETY: the list and its options outlive the call.
        if unsafe { InternetQueryOptionW(null(), INTERNET_OPTION_PER_CONNECTION_OPTION, (&raw mut l).cast(), &mut len) } == 0 {
            bail!("reading the proxy settings: {}", std::io::Error::last_os_error());
        }
        // SAFETY: WinINet filled the strings (or left them null); they are ours to free.
        let take = |o: &INTERNET_PER_CONN_OPTIONW| unsafe {
            let p = o.Value.pszValue;
            if p.is_null() {
                return None;
            }
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            GlobalFree(p.cast());
            Some(s)
        };
        // SAFETY: FLAGS is a number.
        let flags = unsafe { opts[0].Value.dwValue };
        Ok(Saved { flags, server: take(&opts[1]), bypass: take(&opts[2]), pac: take(&opts[3]) })
    }

    fn write(s: &Saved) -> Result<()> {
        let wide = |v: &Option<String>| v.as_deref().map(|s| s.encode_utf16().chain([0]).collect::<Vec<u16>>());
        let (mut server, mut bypass, mut pac) = (wide(&s.server), wide(&s.bypass), wide(&s.pac));
        let ptr = |v: &mut Option<Vec<u16>>| v.as_mut().map_or(null_mut(), |v| v.as_mut_ptr());
        let mut opts = options();
        opts[0].Value.dwValue = s.flags;
        opts[1].Value.pszValue = ptr(&mut server);
        opts[2].Value.pszValue = ptr(&mut bypass);
        opts[3].Value.pszValue = ptr(&mut pac);
        let l = list(&mut opts);
        // SAFETY: the list, its options and their strings outlive the calls.
        unsafe {
            let size = size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32;
            if InternetSetOptionW(null(), INTERNET_OPTION_PER_CONNECTION_OPTION, (&raw const l).cast::<c_void>(), size) == 0 {
                bail!("changing the proxy settings: {}", std::io::Error::last_os_error());
            }
            // Running apps reread the settings.
            InternetSetOptionW(null(), INTERNET_OPTION_SETTINGS_CHANGED, null(), 0);
            InternetSetOptionW(null(), INTERNET_OPTION_REFRESH, null(), 0);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    //! GNOME's settings (gsettings; also read by Firefox and GTK apps on other
    //! desktops) and KDE's (kioslaverc). Other desktops have no system proxy:
    //! apps there need it set by hand.

    use std::process::Command;

    use anyhow::Result;
    use serde::{Deserialize, Serialize};

    const GNOME: [(&str, &str); 8] = [
        ("org.gnome.system.proxy.http", "host"),
        ("org.gnome.system.proxy.http", "port"),
        ("org.gnome.system.proxy.https", "host"),
        ("org.gnome.system.proxy.https", "port"),
        ("org.gnome.system.proxy.socks", "host"),
        ("org.gnome.system.proxy.socks", "port"),
        ("org.gnome.system.proxy", "ignore-hosts"),
        // Last: switching it on applies the rest.
        ("org.gnome.system.proxy", "mode"),
    ];
    const KDE_KEYS: [&str; 5] = ["httpProxy", "httpsProxy", "socksProxy", "NoProxyFor", "ProxyType"];

    #[derive(Debug, Default, Serialize, Deserialize)]
    pub struct Saved {
        /// (schema, key, GVariant text) as `gsettings get` printed it.
        gnome: Vec<(String, String, String)>,
        /// (key, value); empty means the key was not set.
        kde: Vec<(String, String)>,
        /// kreadconfig/kwriteconfig of this KDE version.
        kde_tools: Option<(String, String)>,
    }

    pub fn enable(port: u16, save: impl FnOnce(&Saved) -> Result<()>) -> Result<Option<String>> {
        let mut before = Saved::default();
        if gsettings(&["get", "org.gnome.system.proxy", "mode"]).is_some() {
            for (schema, key) in GNOME {
                if let Some(v) = gsettings(&["get", schema, key]) {
                    before.gnome.push((schema.into(), key.into(), v));
                }
            }
        }
        if let Some((read, write)) = kde_tools() {
            for key in KDE_KEYS {
                let v = Command::new(read)
                    .args(["--file", "kioslaverc", "--group", "Proxy Settings", "--key", key])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                before.kde.push((key.into(), v));
            }
            before.kde_tools = Some((read.into(), write.into()));
        }
        if before.gnome.is_empty() && before.kde_tools.is_none() {
            return Ok(Some(format!(
                "В этом окружении нет системных настроек прокси (они есть в GNOME и KDE). \
                 Укажите прокси 127.0.0.1:{port} (HTTP или SOCKS5) в нужных приложениях вручную."
            )));
        }
        save(&before)?;

        let port_s = port.to_string();
        let ignore = "['localhost', '127.0.0.0/8', '::1', '10.0.0.0/8', '172.16.0.0/12', '192.168.0.0/16', '169.254.0.0/16', '*.local']";
        if !before.gnome.is_empty() {
            for (schema, key) in GNOME {
                let value = match key {
                    "host" => "'127.0.0.1'",
                    "port" => port_s.as_str(),
                    "ignore-hosts" => ignore,
                    _ => "'manual'",
                };
                gsettings(&["set", schema, key, value]);
            }
        }
        if let Some((_, write)) = &before.kde_tools {
            let no_proxy = "localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16,.local";
            for (key, value) in [
                ("httpProxy", format!("http://127.0.0.1 {port}")),
                ("httpsProxy", format!("http://127.0.0.1 {port}")),
                ("socksProxy", format!("socks://127.0.0.1 {port}")),
                ("NoProxyFor", no_proxy.to_string()),
                ("ProxyType", "1".to_string()),
            ] {
                kwrite(write, key, Some(&value));
            }
            kde_reload();
        }
        Ok(None)
    }

    pub fn restore(saved: &Saved) {
        for (schema, key, value) in &saved.gnome {
            gsettings(&["set", schema, key, value]);
        }
        if let Some((_, write)) = &saved.kde_tools {
            for (key, value) in &saved.kde {
                kwrite(write, key, (!value.is_empty()).then_some(value.as_str()));
            }
            kde_reload();
        }
    }

    fn gsettings(args: &[&str]) -> Option<String> {
        let out = Command::new("gsettings").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn kde_tools() -> Option<(&'static str, &'static str)> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        if !desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE")) {
            return None;
        }
        [("kreadconfig6", "kwriteconfig6"), ("kreadconfig5", "kwriteconfig5")]
            .into_iter()
            .find(|(read, _)| Command::new(read).arg("--help").output().is_ok_and(|o| o.status.success()))
    }

    fn kwrite(tool: &str, key: &str, value: Option<&str>) {
        let mut cmd = Command::new(tool);
        cmd.args(["--file", "kioslaverc", "--group", "Proxy Settings", "--key", key]);
        match value {
            Some(v) => cmd.arg(v),
            None => cmd.arg("--delete"),
        };
        let _ = cmd.output();
    }

    /// Running KDE apps reread kioslaverc.
    fn kde_reload() {
        let _ = Command::new("dbus-send")
            .args(["--type=signal", "/KIO/Scheduler", "org.kde.KIO.Scheduler.reparseSlaveConfiguration", "string:"])
            .output();
    }
}

//! Starting DUORAY at login, per user (no admin rights): the Run key on
//! Windows, an XDG autostart entry on Linux, a LaunchAgent on macOS. The entry
//! passes `--autostart`, so DUORAY starts hidden in the tray. It is rewritten
//! on every start while enabled: the path changes with updates (AppImage).

use anyhow::{Context, Result};

pub const ARG: &str = "--autostart";

/// Whether this process was started at login.
pub fn launched() -> bool {
    std::env::args().any(|a| a == ARG)
}

/// The program the entry starts: the AppImage itself rather than its
/// temporary mount, otherwise this executable.
fn program() -> Result<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return Ok(appimage.into());
    }
    std::env::current_exe().context("program path")
}

/// Adds or removes the login entry.
pub fn set(enabled: bool) -> Result<()> {
    imp::set(enabled, &program()?)
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};

    const KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE: &str = "DUORAY";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    pub fn set(enabled: bool, program: &Path) -> Result<()> {
        let (key, value) = (wide(KEY), wide(VALUE));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the calls.
        let err = unsafe {
            if enabled {
                let data = wide(&format!("\"{}\" {}", program.display(), super::ARG));
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    value.as_ptr(),
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            } else {
                match RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) {
                    ERROR_FILE_NOT_FOUND => 0,
                    err => err,
                }
            }
        };
        if err != 0 {
            bail!("автозапуск: ошибка реестра {err}");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    fn plist() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME")?;
        Ok(PathBuf::from(home).join("Library/LaunchAgents/space.dualizm.duoray.plist"))
    }

    fn escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub fn set(enabled: bool, program: &Path) -> Result<()> {
        let path = plist()?;
        if !enabled {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let text = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>space.dualizm.duoray</string>\n\
             <key>ProgramArguments</key><array><string>{}</string><string>{}</string></array>\n\
             <key>RunAtLoad</key><true/>\n\
             </dict></plist>\n",
            escape(&program.display().to_string()),
            super::ARG
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text).context("автозапуск: LaunchAgent")
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};

    fn entry() -> Result<PathBuf> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .context("no config directory")?;
        Ok(config.join("autostart").join("duoray.desktop"))
    }

    /// Exec quoting of the Desktop Entry spec.
    fn quote(s: &str) -> String {
        let mut out = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    }

    pub fn set(enabled: bool, program: &Path) -> Result<()> {
        let path = entry()?;
        if !enabled {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let text = format!(
            "[Desktop Entry]\nType=Application\nName=DUORAY\nExec={} {}\nIcon=duoray\nTerminal=false\n\
             X-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
            quote(&program.display().to_string()),
            super::ARG
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text).context("автозапуск: ~/.config/autostart")
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn quotes_exec_paths() {
            assert_eq!(super::quote("/opt/My Apps/duoray"), "\"/opt/My Apps/duoray\"");
            assert_eq!(super::quote("/a/$b`c\"d"), "\"/a/\\$b\\`c\\\"d\"");
        }
    }
}

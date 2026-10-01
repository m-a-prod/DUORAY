//! Talking to, and installing, the privileged helper.

use std::io::BufReader;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use duoray_core::helper_proto::{self, MIN_PROTOCOL, PROTOCOL, Request, Response, TunRequest};

#[derive(Debug)]
pub enum OpenError {
    /// Not installed or not running.
    Missing,
    /// Installed, but speaks an older protocol.
    Outdated(String),
    Other(anyhow::Error),
}

#[cfg(unix)]
type Conn = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Conn = std::fs::File;

/// An open connection to the helper. The TUN session (if started) lives as
/// long as this value: dropping it makes the helper restore the network.
/// Strictly request → response (see `helper_proto`).
pub struct HelperSession {
    conn: BufReader<Conn>,
    pub version: String,
    pub protocol: u32,
}

impl HelperSession {
    pub fn open() -> Result<Self, OpenError> {
        let conn = connect().map_err(|_| OpenError::Missing)?;
        let mut s = Self { conn: BufReader::new(conn), version: String::new(), protocol: 0 };
        match s.call(&Request::Hello) {
            Ok(Response::Hello { protocol, version }) if (MIN_PROTOCOL..=PROTOCOL).contains(&protocol) => {
                s.version = version;
                s.protocol = protocol;
                Ok(s)
            }
            Ok(Response::Hello { version, .. }) => Err(OpenError::Outdated(version)),
            Ok(other) => Err(OpenError::Other(anyhow!("unexpected reply {other:?}"))),
            // An old helper closes or answers garbage to requests it does not know.
            Err(e) => Err(OpenError::Other(e)),
        }
    }

    pub fn start(&mut self, req: TunRequest) -> Result<String> {
        match self.call(&Request::Start(req))? {
            Response::Started { tun } => Ok(tun),
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        match self.call(&Request::Stop)? {
            Response::Stopped => Ok(()),
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    /// `Ok(None)` while the TUN is up; `Ok(Some(reason))` once it went down.
    pub fn status(&mut self) -> Result<Option<String>> {
        match self.call(&Request::Status)? {
            Response::Status { running: true, .. } => Ok(None),
            Response::Status { error, .. } => Ok(Some(error.unwrap_or_else(|| "туннель остановлен".into()))),
            Response::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply {other:?}"),
        }
    }

    fn call(&mut self, req: &Request) -> Result<Response> {
        helper_proto::write_msg(self.conn.get_mut(), req).context("помощник не отвечает")?;
        helper_proto::read_msg(&mut self.conn)
            .context("помощник не отвечает")?
            .context("помощник закрыл соединение")
    }
}

#[cfg(unix)]
fn connect() -> std::io::Result<Conn> {
    let s = std::os::unix::net::UnixStream::connect(helper_proto::SOCKET_PATH)?;
    // Start can take a while (routes, DNS); nothing should take longer than this.
    s.set_read_timeout(Some(Duration::from_secs(45)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(s)
}

#[cfg(windows)]
fn connect() -> std::io::Result<Conn> {
    // All pipe instances busy for a moment: retry briefly (ERROR_PIPE_BUSY = 231).
    for _ in 0..20 {
        match std::fs::OpenOptions::new().read(true).write(true).open(helper_proto::PIPE_NAME) {
            Err(e) if e.raw_os_error() == Some(231) => std::thread::sleep(Duration::from_millis(100)),
            other => return other,
        }
    }
    std::fs::OpenOptions::new().read(true).write(true).open(helper_proto::PIPE_NAME)
}

// ── Installation ────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod install_impl {
    use super::*;

    const LABEL: &str = "space.dualizm.duoray.helper";
    const DEST: &str = "/Library/PrivilegedHelperTools/space.dualizm.duoray.helper";
    const PLIST: &str = "/Library/LaunchDaemons/space.dualizm.duoray.helper.plist";
    const LOG: &str = "/var/log/duoray-helper.log";

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn run_as_admin(script: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!("duoray-helper-{}.sh", std::process::id()));
        std::fs::write(&path, script)?;
        let p = path.to_string_lossy().to_string();
        if p.contains('"') || p.contains('\\') {
            bail!("unexpected temp path {p}");
        }
        let apple = format!(
            "do shell script \"/bin/sh {}\" with prompt \"DUORAY устанавливает помощник для VPN-туннеля. Это нужно один раз.\" with administrator privileges",
            shell_quote(&p)
        );
        let out = std::process::Command::new("/usr/bin/osascript").args(["-e", &apple]).output()?;
        let _ = std::fs::remove_file(&path);
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("-128") {
                bail!("установка отменена");
            }
            bail!("установка не удалась: {}", err.trim());
        }
        Ok(())
    }

    pub fn install() -> Result<()> {
        let src = std::env::current_exe()?.with_file_name("duoray-helper");
        if !src.exists() {
            bail!("не найден {} (соберите workspace целиком)", src.display());
        }
        let uid = String::from_utf8(std::process::Command::new("/usr/bin/id").arg("-u").output()?.stdout)?
            .trim()
            .parse::<u32>()?;
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{DEST}</string><string>--allowed-uid</string><string>{uid}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{LOG}</string>
  <key>StandardOutPath</key><string>{LOG}</string>
</dict>
</plist>
"#
        );
        let script = format!(
            "set -e\n\
             launchctl bootout system/{LABEL} 2>/dev/null || true\n\
             install -d -m 755 -o root -g wheel /Library/PrivilegedHelperTools\n\
             install -m 755 -o root -g wheel {src} {DEST}\n\
             cat > {PLIST} <<'DUORAY_PLIST'\n{plist}DUORAY_PLIST\n\
             chown root:wheel {PLIST}\n\
             chmod 644 {PLIST}\n\
             launchctl bootstrap system {PLIST}\n",
            src = shell_quote(&src.to_string_lossy()),
        );
        run_as_admin(&script)
    }

    pub fn uninstall() -> Result<()> {
        run_as_admin(&format!(
            "launchctl bootout system/{LABEL} 2>/dev/null || true\n\
             rm -f {PLIST} {DEST} {sock}\n",
            sock = helper_proto::SOCKET_PATH
        ))
    }
}

#[cfg(target_os = "macos")]
pub use install_impl::{install, uninstall};

#[cfg(target_os = "linux")]
mod install_impl {
    use super::*;

    const UNIT: &str = "/etc/systemd/system/duoray-helper.service";
    /// Where a helper not shipped by a package gets copied.
    const LOCAL_HELPER: &str = "/usr/local/lib/duoray/duoray-helper";
    const PACKAGED_HELPER: &str = "/usr/lib/duoray/duoray-helper";

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    /// Runs a script as root through polkit (graphical password prompt).
    fn run_as_admin(script: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!("duoray-helper-{}.sh", std::process::id()));
        std::fs::write(&path, script)?;
        let out = std::process::Command::new("pkexec")
            .arg("/bin/sh")
            .arg(&path)
            .output()
            .map_err(|_| anyhow!("не найден pkexec (polkit). Установите polkit и агент аутентификации"))?;
        let _ = std::fs::remove_file(&path);
        match out.status.code() {
            Some(0) => Ok(()),
            // 126: the user dismissed the dialog, 127: not authorized.
            Some(126) | Some(127) => bail!("установка отменена"),
            _ => bail!("установка не удалась: {}", String::from_utf8_lossy(&out.stderr).trim()),
        }
    }

    pub fn install() -> Result<()> {
        let uid = String::from_utf8(std::process::Command::new("id").arg("-u").output()?.stdout)?
            .trim()
            .parse::<u32>()?;
        // A package already ships a root-owned helper; otherwise copy ours.
        let (helper, copy) = if std::path::Path::new(PACKAGED_HELPER).is_file() {
            (PACKAGED_HELPER.to_string(), String::new())
        } else {
            let src = std::env::current_exe()?.with_file_name("duoray-helper");
            if !src.exists() {
                bail!("не найден {} (соберите workspace целиком)", src.display());
            }
            (
                LOCAL_HELPER.to_string(),
                format!(
                    "install -d -m 755 /usr/local/lib/duoray\ninstall -m 755 -o root -g root {} {LOCAL_HELPER}\n",
                    shell_quote(&src.to_string_lossy())
                ),
            )
        };
        // xray runs as the user but must bind its sockets to the uplink
        // (SO_BINDTODEVICE), or `direct` traffic loops back into the TUN.
        let setcap = match crate::connection::find_xray() {
            Ok((xray, _)) => format!(
                "setcap cap_net_raw,cap_net_admin+ep {} || echo 'setcap failed' >&2\n",
                shell_quote(&xray.to_string_lossy())
            ),
            Err(_) => String::new(),
        };
        let unit = format!(
            "[Unit]\nDescription=DUORAY TUN helper\nAfter=network.target\n\n\
             [Service]\nExecStart={helper} --allowed-uid {uid}\nRestart=always\nRestartSec=2\n\n\
             [Install]\nWantedBy=multi-user.target\n"
        );
        let script = format!(
            "set -e\n{copy}{setcap}cat > {UNIT} <<'DUORAY_UNIT'\n{unit}DUORAY_UNIT\n\
             systemctl daemon-reload\n\
             systemctl enable duoray-helper.service\n\
             systemctl restart duoray-helper.service\n"
        );
        run_as_admin(&script)
    }

    pub fn uninstall() -> Result<()> {
        run_as_admin(&format!(
            "systemctl disable --now duoray-helper.service 2>/dev/null || true\n\
             rm -f {UNIT} {LOCAL_HELPER} {sock}\n\
             systemctl daemon-reload\n",
            sock = helper_proto::SOCKET_PATH
        ))
    }
}

#[cfg(target_os = "linux")]
pub use install_impl::{install, uninstall};

#[cfg(windows)]
pub use crate::windows_helper::{install, uninstall};

/// Human-readable helper state for the settings window.
pub fn describe() -> String {
    match HelperSession::open() {
        Ok(s) => format!("Установлен, версия {}. Подключение не требует пароля администратора.", s.version),
        Err(OpenError::Missing) => "Не установлен. Понадобится один раз при первом подключении.".into(),
        Err(OpenError::Outdated(v)) => format!("Установлена устаревшая версия {v}, нужно переустановить."),
        Err(OpenError::Other(e)) => format!("Ошибка: {e:#}"),
    }
}

pub fn install_and_wait() -> Result<()> {
    install()?;
    wait_ready(Duration::from_secs(10)).map_err(|e| match e {
        OpenError::Missing => anyhow!("помощник установлен, но не запустился (см. /var/log/duoray-helper.log)"),
        OpenError::Outdated(v) => anyhow!("запустилась старая версия помощника {v}"),
        OpenError::Other(e) => e,
    })
}

/// Waits for a freshly installed helper to come up.
pub fn wait_ready(timeout: Duration) -> Result<(), OpenError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match HelperSession::open() {
            Ok(_) => return Ok(()),
            Err(OpenError::Missing) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(e),
        }
    }
}

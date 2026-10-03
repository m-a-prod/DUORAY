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
            // Linux helpers before 0.2.0 lack the fixes that make the tunnel work there
            // (rp_filter, host firewall, DNS redirect): same protocol, but replace them.
            Ok(Response::Hello { version, .. }) if cfg!(target_os = "linux") && older(&version, "0.2.0") => {
                Err(OpenError::Outdated(version))
            }
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

/// `a < b` for dotted numeric versions.
fn older(a: &str, b: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parse(a) < parse(b)
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
        check_fresh(&src)?;
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
#[cfg(not(target_os = "linux"))]
const LOG_HINT: &str = "/var/log/duoray-helper.log";

#[cfg(target_os = "linux")]
mod install_impl {
    use super::*;

    const SERVICE: &str = "duoray-helper";
    const UNIT: &str = "/etc/systemd/system/duoray-helper.service";
    const OPENRC: &str = "/etc/init.d/duoray-helper";
    const RUNIT: &str = "/etc/sv/duoray-helper";
    /// Where a helper not shipped by a package gets copied. libexec: SELinux
    /// (Fedora) lets services execute bin_t files from there, not lib_t ones.
    const LOCAL_HELPER: &str = "/usr/local/libexec/duoray/duoray-helper";
    /// Helpers installed by a package or by packaging/linux/install.sh.
    const PACKAGED_HELPERS: &[&str] = &["/usr/libexec/duoray/duoray-helper", "/usr/lib/duoray/duoray-helper"];
    /// Paths older builds installed to.
    const OLD_HELPERS: &[&str] = &["/usr/local/lib/duoray/duoray-helper"];

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', r"'\''"))
    }

    fn is_root() -> bool {
        // SAFETY: plain syscall.
        unsafe { libc::geteuid() == 0 }
    }

    /// The desktop user the helper serves: the real user even under sudo/pkexec.
    fn user_uid() -> Result<u32> {
        if is_root() {
            for var in ["PKEXEC_UID", "SUDO_UID"] {
                if let Some(uid) = std::env::var(var).ok().and_then(|v| v.parse().ok()) {
                    return Ok(uid);
                }
            }
            bail!("запустите через sudo от своего пользователя: sudo duoray --install-helper");
        }
        // SAFETY: plain syscall.
        Ok(unsafe { libc::getuid() })
    }

    /// Runs a script as root: directly when already root (`sudo duoray
    /// --install-helper`), otherwise through polkit (graphical password prompt).
    fn run_as_admin(script: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!("duoray-helper-{}.sh", std::process::id()));
        crate::connection::write_private(&path, script.as_bytes())?;
        let out = if is_root() {
            std::process::Command::new("/bin/sh").arg(&path).output()?
        } else {
            std::process::Command::new("pkexec").arg("/bin/sh").arg(&path).output().map_err(|_| {
                anyhow!("не найден pkexec (polkit). Установите polkit или выполните в терминале: sudo duoray --install-helper")
            })?
        };
        let _ = std::fs::remove_file(&path);
        let err = String::from_utf8_lossy(&out.stderr);
        match out.status.code() {
            Some(0) => Ok(()),
            _ if err.contains("authentication agent") || err.contains("No session for cookie") => bail!(
                "нет агента polkit, который спросил бы пароль. Запустите агент вашего окружения \
                 или выполните в терминале: sudo duoray --install-helper"
            ),
            // 126: the user dismissed the dialog, 127: not authorized.
            Some(126) | Some(127) if !is_root() => bail!("установка отменена"),
            _ => bail!("установка не удалась: {}", err.trim()),
        }
    }

    /// Before 5.7 binding a socket to a device (xray's `sockopt.interface`)
    /// needs CAP_NET_RAW; without it `direct` traffic loops into the TUN.
    fn needs_net_raw() -> bool {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        let mut parts = release.trim().split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse::<u32>().ok());
        let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        (major, minor) < (5, 7)
    }

    pub fn install() -> Result<()> {
        let uid = user_uid()?;
        // A package already ships a root-owned helper; otherwise copy ours.
        // An AppImage's FUSE mount is not readable by root: hand the helper over
        // through a temporary copy.
        let mut staged = None;
        let (helper, copy) = match PACKAGED_HELPERS.iter().find(|p| std::path::Path::new(p).is_file()) {
            Some(p) => (p.to_string(), String::new()),
            None => {
                let src = std::env::current_exe()?.with_file_name("duoray-helper");
                if !src.exists() {
                    bail!("не найден {} (соберите workspace целиком)", src.display());
                }
                check_fresh(&src)?;
                let tmp = std::env::temp_dir().join(format!("duoray-helper-{}.bin", std::process::id()));
                std::fs::copy(&src, &tmp).context("копирование помощника")?;
                let copy = format!(
                    "install -d -m 755 /usr/local/libexec/duoray\n\
                     install -m 755 -o root -g root {} {LOCAL_HELPER}.new\n\
                     mv -f {LOCAL_HELPER}.new {LOCAL_HELPER}\n\
                     command -v restorecon >/dev/null 2>&1 && restorecon -F {LOCAL_HELPER} || true\n",
                    shell_quote(&tmp.to_string_lossy())
                );
                staged = Some(tmp);
                (LOCAL_HELPER.to_string(), copy)
            }
        };
        let setcap = match crate::connection::find_xray() {
            Ok((xray, _)) if needs_net_raw() => format!(
                "setcap cap_net_raw+ep {} || echo 'setcap failed' >&2\n",
                shell_quote(&xray.to_string_lossy())
            ),
            _ => String::new(),
        };
        let result = run_as_admin(&install_script(&helper, &copy, &setcap, uid));
        if let Some(tmp) = staged {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }

    fn install_script(helper: &str, copy: &str, setcap: &str, uid: u32) -> String {
        let old: Vec<String> = OLD_HELPERS.iter().map(|p| shell_quote(p)).collect();
        let unit = format!(
            "[Unit]\nDescription=DUORAY TUN helper\nAfter=network.target\n\n\
             [Service]\nExecStart={helper} --allowed-uid {uid}\nRestart=always\nRestartSec=2\n\n\
             [Install]\nWantedBy=multi-user.target\n"
        );
        let openrc = format!(
            "#!/sbin/openrc-run\n\
             description=\"DUORAY TUN helper\"\n\
             command={helper}\n\
             command_args=\"--allowed-uid {uid}\"\n\
             supervisor=supervise-daemon\n\
             respawn_delay=2\n\
             output_log=/var/log/duoray-helper.log\n\
             error_log=/var/log/duoray-helper.log\n\
             depend() {{ need net; }}\n"
        );
        let runit = format!("#!/bin/sh\nexec {helper} --allowed-uid {uid} 2>&1\n");
        // One service manager: whichever init the system booted with.
        format!(
            "set -e\n{copy}{setcap}rm -f {old}\n\
             if [ -d /run/systemd/system ]; then\n\
             cat > {UNIT} <<'DUORAY_UNIT'\n{unit}DUORAY_UNIT\n\
             systemctl daemon-reload\n\
             systemctl enable {SERVICE}.service\n\
             systemctl restart {SERVICE}.service\n\
             elif command -v openrc-run >/dev/null 2>&1; then\n\
             cat > {OPENRC} <<'DUORAY_RC'\n{openrc}DUORAY_RC\n\
             chmod 755 {OPENRC}\n\
             rc-update add {SERVICE} default\n\
             rc-service {SERVICE} restart\n\
             elif command -v sv >/dev/null 2>&1; then\n\
             mkdir -p {RUNIT}\n\
             cat > {RUNIT}/run <<'DUORAY_RUN'\n{runit}DUORAY_RUN\n\
             chmod 755 {RUNIT}/run\n\
             for d in /var/service /run/runit/service /etc/runit/runsvdir/default; do\n\
             if [ -d \"$d\" ]; then ln -sfn {RUNIT} \"$d/{SERVICE}\"; break; fi\n\
             done\n\
             sleep 1; sv restart {SERVICE} || true\n\
             else\n\
             echo 'неизвестная система инициализации (нужен systemd, OpenRC или runit)' >&2; exit 1\n\
             fi\n",
            old = old.join(" "),
        )
    }

    pub fn uninstall() -> Result<()> {
        let helpers: Vec<String> = std::iter::once(LOCAL_HELPER).chain(OLD_HELPERS.iter().copied()).map(shell_quote).collect();
        run_as_admin(&format!(
            "if [ -d /run/systemd/system ]; then\n\
             systemctl disable --now {SERVICE}.service 2>/dev/null || true\n\
             rm -f {UNIT}\n\
             systemctl daemon-reload\n\
             fi\n\
             if [ -x {OPENRC} ]; then rc-service {SERVICE} stop || true; rc-update del {SERVICE} default || true; rm -f {OPENRC}; fi\n\
             if [ -d {RUNIT} ]; then sv stop {SERVICE} || true; rm -f /var/service/{SERVICE} /run/runit/service/{SERVICE} /etc/runit/runsvdir/default/{SERVICE}; rm -rf {RUNIT}; fi\n\
             rm -f {helpers} {sock}\n",
            helpers = helpers.join(" "),
            sock = helper_proto::SOCKET_PATH
        ))
    }

    /// Where to look when the helper does not come up.
    pub const LOG_HINT: &str = "journalctl -u duoray-helper";

    #[cfg(test)]
    mod tests {
        #[test]
        fn install_script_is_valid_shell() {
            let script = super::install_script("/usr/local/libexec/duoray/duoray-helper", "true\n", "", 1000);
            assert!(script.contains("ExecStart=/usr/local/libexec/duoray/duoray-helper --allowed-uid 1000"));
            let out = std::process::Command::new("sh").args(["-n", "-c", &script]).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
    }
}

#[cfg(target_os = "linux")]
use install_impl::LOG_HINT;
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
        OpenError::Missing => anyhow!("помощник установлен, но не запустился (см. {LOG_HINT})"),
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

/// Refuses to install a helper binary older than this app (e.g. after
/// `cargo run -p duoray-gui`, which does not rebuild the helper): it would be
/// reported as outdated again right after the password prompt.
#[cfg(unix)]
fn check_fresh(helper: &std::path::Path) -> Result<()> {
    use std::io::Read;
    // Helpers before protocol 3 ignore the flag and try to serve: give up on them.
    let mut child = std::process::Command::new(helper)
        .arg("--protocol")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = child.wait();
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    let protocol: u32 = out.trim().parse().unwrap_or(0);
    if protocol < PROTOCOL {
        bail!(
            "помощник рядом с DUORAY устарел (протокол {protocol}, нужен {PROTOCOL}). \
             Соберите его: cargo build --release -p duoray-helper"
        );
    }
    Ok(())
}

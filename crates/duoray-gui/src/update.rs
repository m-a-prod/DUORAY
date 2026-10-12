//! Self-update from the hub: a signed manifest lists one build per platform.
//!
//! The manifest is signed with an Ed25519 key that never leaves the release
//! machine (`packaging/release.sh`); the public half is below. A download is
//! used only if the manifest signature checks out and the file matches the
//! manifest's SHA-256 and size, so a compromised server cannot push code.
//!
//! Install per platform: the Windows installer runs silently (one UAC prompt)
//! and starts the new version; an AppImage replaces itself; an rpm, deb or
//! pacman install hands the package to its package manager through pkexec;
//! anything else (macOS disk images, Linux installs from install.sh) is
//! downloaded, verified and opened, or the release page is shown.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const USER_AGENT: &str = concat!("Duoray/", env!("CARGO_PKG_VERSION"));
/// Git commit of this build (see build.rs).
pub const BUILD: &str = env!("DUORAY_BUILD");

/// Public half of ~/.config/duoray-release/update-signing.pem.
const PUBLIC_KEY: [u8; 32] = hex32("e2d7d74a8fffaee8f4c3bb9f566f7bb2a533afd23755eb671bdc6fe280f478ab");

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Git commit the builds were made from.
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub notes: String,
    /// Where to send users whose platform has no asset.
    #[serde(default)]
    pub page: String,
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub file: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Install {
    /// Windows NSIS installer, run with /S.
    Setup,
    /// Replace this AppImage file.
    AppImage(PathBuf),
    /// Installed by a package manager: upgrade through it.
    Package(Package),
    /// Download, verify and open (macOS .dmg).
    Open,
    /// Only point to the release page.
    Notify,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Package {
    Rpm,
    Deb,
    Pacman,
}

impl Package {
    fn key(self) -> &'static str {
        match self {
            Package::Rpm => "rpm",
            Package::Deb => "deb",
            Package::Pacman => "pacman",
        }
    }

    /// The package that owns this executable, if it is our `duoray`.
    #[cfg(target_os = "linux")]
    fn of_this_install() -> Option<Package> {
        let exe = std::env::current_exe().ok()?;
        let exe = exe.to_str()?;
        let owner = |program: &str, args: &[&str]| {
            let out = std::process::Command::new(program).args(args).output().ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        if owner("pacman", &["-Qqo", exe]).as_deref() == Some("duoray") {
            Some(Package::Pacman)
        } else if owner("rpm", &["-qf", "--qf", "%{NAME}", exe]).as_deref() == Some("duoray") {
            Some(Package::Rpm)
        } else if owner("dpkg-query", &["-S", exe]).is_some_and(|o| o.starts_with("duoray:")) {
            Some(Package::Deb)
        } else {
            None
        }
    }

    /// The root command that installs `file`, in the first package manager
    /// found (dnf resolves new dependencies, rpm alone does not).
    fn command(self, file: &Path) -> Vec<String> {
        let file = file.display().to_string();
        let found = |name: &str| which(name).is_some();
        let args: &[&str] = match self {
            Package::Rpm if found("dnf") => &["dnf", "install", "-y"],
            Package::Rpm if found("zypper") => &["zypper", "--non-interactive", "install", "--allow-unsigned-rpm"],
            Package::Rpm => &["rpm", "-U", "--replacepkgs"],
            Package::Deb if found("apt-get") => &["apt-get", "install", "-y"],
            Package::Deb => &["dpkg", "-i"],
            Package::Pacman => &["pacman", "-U", "--noconfirm"],
        };
        args.iter().map(|a| a.to_string()).chain([file]).collect()
    }
}

/// Full path of a program on PATH (pkexec wants one).
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// Shell quoting for the command shown to the user.
fn shell_quote(arg: &str) -> String {
    if arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=".contains(c)) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// A newer version for this platform.
#[derive(Debug, Clone)]
pub struct Found {
    /// The hub that served the manifest; downloads try it first.
    pub hub: String,
    pub version: String,
    pub notes: String,
    pub page: String,
    pub asset: Option<Asset>,
    pub install: Install,
}

/// Manifest key of this build and how it updates: `<os>-<arch>-<format>`.
pub fn platform() -> (String, Install) {
    let arch = std::env::consts::ARCH;
    if cfg!(target_os = "windows") {
        (format!("windows-{arch}-setup"), Install::Setup)
    } else if cfg!(target_os = "macos") {
        (format!("macos-{arch}-dmg"), Install::Open)
    } else if let Some(path) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
        (format!("linux-{arch}-appimage"), Install::AppImage(PathBuf::from(path)))
    } else if let Some(package) = installed_package() {
        (format!("linux-{arch}-{}", package.key()), Install::Package(package))
    } else {
        (format!("linux-{arch}-appimage"), Install::Notify)
    }
}

/// Asked once: it runs the package manager.
fn installed_package() -> Option<Package> {
    #[cfg(target_os = "linux")]
    {
        static FOUND: std::sync::OnceLock<Option<Package>> = std::sync::OnceLock::new();
        *FOUND.get_or_init(Package::of_this_install)
    }
    #[cfg(not(target_os = "linux"))]
    None
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into()
}

fn get(url: &str) -> Result<Vec<u8>> {
    let mut body = agent(Duration::from_secs(20))
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("запрос {}", url.rsplit('/').next().unwrap_or(url)))?
        .into_body();
    Ok(body.with_config().limit(1024 * 1024).read_to_vec()?)
}

/// Asks the hubs for a newer version, mirrors in turn. `Ok(None)`: up to date.
pub fn check() -> Result<Option<Found>> {
    let mut last = anyhow::anyhow!("нет серверов обновлений");
    for hub in crate::diag::hubs() {
        match check_at(&hub) {
            Ok(m) => {
                crate::diag::hub_ok(&hub);
                return Ok(select(m, env!("CARGO_PKG_VERSION"), BUILD, platform()).map(|f| Found { hub, ..f }));
            }
            // A mirror with a bad signature is skipped like a dead one.
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn check_at(hub: &str) -> Result<Manifest> {
    let base = format!("{hub}/v1/update");
    let manifest = get(&format!("{base}/manifest.json"))?;
    let sig = get(&format!("{base}/manifest.sig"))?;
    verify(&manifest, &String::from_utf8_lossy(&sig), &PUBLIC_KEY)?;
    serde_json::from_slice(&manifest).context("манифест обновления")
}

/// Offered: a newer version, or the same version built from another commit
/// (a fix shipped without a version bump). The same build re-uploaded, or
/// anything older, is not.
fn select(m: Manifest, current: &str, build: &str, (key, install): (String, Install)) -> Option<Found> {
    let same_version = !newer(&m.version, current) && !newer(current, &m.version);
    let rebuilt = same_version && !m.build.is_empty() && m.build != build;
    if !newer(&m.version, current) && !rebuilt {
        return None;
    }
    let asset = m.assets.get(&key).cloned();
    let install = if asset.is_some() { install } else { Install::Notify };
    Some(Found { hub: String::new(), version: m.version, notes: m.notes, page: m.page, asset, install })
}

fn verify(data: &[u8], sig_hex: &str, key: &[u8; 32]) -> Result<()> {
    let sig = hex(sig_hex.trim()).context("подпись манифеста")?;
    let sig = Signature::from_slice(&sig).context("подпись манифеста")?;
    VerifyingKey::from_bytes(key)?
        .verify_strict(data, &sig)
        .map_err(|_| anyhow::anyhow!("подпись манифеста не сходится"))
}

/// `a > b` for dotted versions; a pre-release suffix ("0.4.0-rc1") ranks lower.
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| {
        let (core, pre) = v.trim().trim_start_matches('v').split_once('-').map_or((v.trim().trim_start_matches('v'), None), |(c, p)| (c, Some(p.to_string())));
        let nums: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
        (nums, pre)
    };
    let ((mut na, pa), (mut nb, pb)) = (parse(a), parse(b));
    let len = na.len().max(nb.len());
    na.resize(len, 0);
    nb.resize(len, 0);
    match na.cmp(&nb) {
        std::cmp::Ordering::Equal => match (pa, pb) {
            (None, Some(_)) => true,
            (Some(x), Some(y)) => x > y,
            _ => false,
        },
        o => o.is_gt(),
    }
}

/// Downloads `asset` into `dir` (reusing a verified earlier download), from
/// `hub` first and then the mirrors. `progress(done, total)` reports bytes.
pub fn download(asset: &Asset, hub: &str, dir: &Path, progress: impl Fn(u64, u64)) -> Result<PathBuf> {
    let mut order = vec![hub.to_string()];
    order.extend(crate::diag::hubs().into_iter().filter(|h| h != hub));
    let mut last = anyhow::anyhow!("нет серверов обновлений");
    for h in order.iter().filter(|h| !h.is_empty()) {
        match download_from(asset, h, dir, &progress) {
            Ok(path) => return Ok(path),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn download_from(asset: &Asset, hub: &str, dir: &Path, progress: &impl Fn(u64, u64)) -> Result<PathBuf> {
    if asset.file.is_empty() || !asset.file.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        bail!("неверное имя файла в манифесте");
    }
    std::fs::create_dir_all(dir)?;
    let target = dir.join(&asset.file);
    if sha256_file(&target).is_ok_and(|h| h.eq_ignore_ascii_case(&asset.sha256)) {
        return Ok(target);
    }
    let url = format!("{hub}/v1/update/files/{}", asset.file);
    let mut body = agent(Duration::from_secs(30 * 60))
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .call()
        .context("загрузка обновления")?
        .into_body();
    let part = dir.join(format!("{}.part", asset.file));
    let mut out = std::fs::File::create(&part)?;
    let mut reader = body.as_reader();
    let (mut hasher, mut done, mut buf) = (Sha256::new(), 0u64, vec![0u8; 256 * 1024]);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > asset.size {
            bail!("файл обновления больше заявленного");
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        progress(done, asset.size);
    }
    out.sync_all()?;
    drop(out);
    let hash = to_hex(&hasher.finalize());
    if done != asset.size || !hash.eq_ignore_ascii_case(&asset.sha256) {
        let _ = std::fs::remove_file(&part);
        bail!("файл обновления повреждён (контрольная сумма не сходится)");
    }
    std::fs::rename(&part, &target)?;
    Ok(target)
}

/// Starts installing a verified download. For `Setup` and `AppImage` the
/// caller should quit right after: the new version starts by itself.
pub fn install(file: &Path, how: &Install) -> Result<()> {
    match how {
        Install::Setup => run_setup(file),
        Install::AppImage(current) => {
            // Same directory, so the final rename is atomic; the running
            // AppImage keeps its (now unlinked) file until it exits.
            let staged = current.with_extension("update");
            std::fs::copy(file, &staged).context("нет прав на запись рядом с AppImage")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&staged, current)?;
            std::process::Command::new(current).arg("--wait-for-instance").spawn().context("запуск новой версии")?;
            Ok(())
        }
        Install::Package(package) => install_package(*package, file),
        Install::Open => {
            crate::open_path(file);
            Ok(())
        }
        Install::Notify => Ok(()),
    }
}

/// The package manager upgrades in place, then the new version is started
/// (`/usr/bin/duoray` from the package, this executable's path may be gone).
/// Without polkit, or when the prompt is dismissed, the command goes to the
/// clipboard to paste into a terminal.
fn install_package(package: Package, file: &Path) -> Result<()> {
    let command = package.command(file);
    let shown = format!("sudo {}", command.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
    let fallback = |why: String| {
        let copied = arboard::Clipboard::new().and_then(|mut c| c.set_text(shown.clone())).is_ok();
        let tail = if copied { "команда скопирована, вставьте её в терминал" } else { "выполните в терминале" };
        anyhow::anyhow!("{why}; {tail}: {shown}")
    };
    let (Some(pkexec), Some(program)) = (which("pkexec"), which(&command[0])) else {
        return Err(fallback("нет pkexec".into()));
    };
    let status = std::process::Command::new(pkexec)
        .arg(program)
        .args(&command[1..])
        .status()
        .context("запуск pkexec")?;
    match status.code() {
        Some(0) => {}
        // pkexec: 126 the password prompt was dismissed, 127 no polkit agent.
        Some(126 | 127) => return Err(fallback("нет прав администратора".into())),
        _ => return Err(fallback(format!("{} завершился с ошибкой ({status})", command[0]))),
    }
    let program = ["/usr/bin/duoray", "/usr/lib/duoray/duoray"].into_iter().map(Path::new).find(|p| p.exists());
    let program = program.map(Path::to_path_buf).or_else(|| std::env::current_exe().ok()).context("путь к DUORAY")?;
    std::process::Command::new(program).arg("--wait-for-instance").spawn().context("запуск новой версии")?;
    Ok(())
}

#[cfg(windows)]
fn run_setup(file: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let (verb, path, args) = (wide("open".as_ref()), wide(file.as_os_str()), wide("/S".as_ref()));
    // ShellExecute, not CreateProcess: the installer needs elevation (UAC).
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), path.as_ptr(), args.as_ptr(), std::ptr::null(), SW_SHOWNORMAL) };
    if r as isize <= 32 {
        bail!("установщик не запустился (код {})", r as isize);
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_setup(_file: &Path) -> Result<()> {
    bail!("установщик Windows на этой системе не запускается")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("нечётная длина");
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("не hex")).collect()
}

const fn hex_digit(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("bad hex"),
    }
}

const fn hex32(s: &str) -> [u8; 32] {
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = hex_digit(b[2 * i]) << 4 | hex_digit(b[2 * i + 1]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn versions() {
        assert!(newer("0.3.1", "0.3.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99.99"));
        assert!(!newer("0.3.0", "0.3.0"));
        assert!(!newer("0.2.9", "0.3.0"));
        assert!(newer("0.4.0", "0.4.0-rc1"));
        assert!(!newer("0.4.0-rc1", "0.4.0"));
        assert!(newer("v0.3.1", "0.3.0"));
    }

    #[test]
    fn signature_and_selection() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public = key.verifying_key().to_bytes();
        let manifest = br#"{"version":"0.4.0","page":"https://x","assets":{"windows-x86_64-setup":{"file":"a.exe","sha256":"00","size":1}}}"#;
        let sig = to_hex(&key.sign(manifest).to_bytes());
        assert!(verify(manifest, &sig, &public).is_ok());
        let mut tampered = manifest.to_vec();
        tampered[13] = b'5';
        assert!(verify(&tampered, &sig, &public).is_err(), "edited manifest");
        assert!(verify(manifest, &sig, &PUBLIC_KEY).is_err(), "other key");
        assert!(verify(manifest, "zz", &public).is_err());

        let m: Manifest = serde_json::from_slice(manifest).unwrap();
        let win = ("windows-x86_64-setup".to_string(), Install::Setup);
        let found = select(m.clone(), "0.3.0", "abc", win.clone()).unwrap();
        assert_eq!((found.install, found.asset.unwrap().file.as_str()), (Install::Setup, "a.exe"));
        let mac = select(m.clone(), "0.3.0", "abc", ("macos-aarch64-dmg".into(), Install::Open)).unwrap();
        assert_eq!((mac.install, mac.asset.is_none()), (Install::Notify, true));
        assert!(select(m.clone(), "0.4.0", "abc", win.clone()).is_none(), "same version, manifest without build");
        assert!(select(m.clone(), "0.5.0", "abc", win.clone()).is_none(), "older than installed");

        // Same version: offered only when built from another commit.
        let rebuilt = Manifest { build: "def".into(), ..m.clone() };
        assert!(select(rebuilt.clone(), "0.4.0", "abc", win.clone()).is_some());
        assert!(select(rebuilt.clone(), "0.4.0", "def", win.clone()).is_none(), "re-uploaded same build");
        assert!(select(rebuilt, "0.5.0", "abc", win).is_none(), "never a downgrade");
    }

    #[test]
    fn package_commands() {
        let file = Path::new("/home/u/.local/share/duoray/updates/duoray_0.4.0-1_amd64.deb");
        let deb = Package::Deb.command(file);
        assert_eq!(deb.last().map(String::as_str), Some(file.to_str().unwrap()));
        assert!(matches!(deb[0].as_str(), "apt-get" | "dpkg"));
        assert_eq!(Package::Pacman.command(file)[..3], ["pacman", "-U", "--noconfirm"]);
        assert_eq!(shell_quote("/tmp/a b/it's.rpm"), "'/tmp/a b/it'\\''s.rpm'");
        assert_eq!(shell_quote("/usr/x-1.rpm"), "/usr/x-1.rpm");
        let m: Manifest = serde_json::from_str(
            r#"{"version":"0.4.0","assets":{"linux-x86_64-rpm":{"file":"duoray-0.4.0-1.x86_64.rpm","sha256":"00","size":1}}}"#,
        )
        .unwrap();
        let rpm = ("linux-x86_64-rpm".to_string(), Install::Package(Package::Rpm));
        assert_eq!(select(m, "0.3.6", "abc", rpm).unwrap().install, Install::Package(Package::Rpm));
    }

    #[test]
    fn download_checks_name() {
        let a = Asset { file: "../x".into(), sha256: String::new(), size: 0 };
        assert!(download(&a, "https://h", Path::new("/nonexistent"), |_, _| {}).is_err());
    }
}

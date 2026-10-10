//! Error reports for debugging, sent only with the user's consent.
//!
//! Every text that leaves the machine goes through [`sanitize`]: links, IP
//! addresses, host names, UUIDs, e-mails, keys and the home directory are
//! replaced with placeholders, so subscription links, server addresses and
//! credentials never reach the hub. Reports are queued as files and sent in
//! the background, so a crash or a broken network loses nothing.
//!
//! Automatic reports need `Settings::telemetry == Some(true)`; a report the
//! user sends from "Сообщить о проблеме" is an explicit action and is sent
//! regardless (the dialog shows exactly what goes out).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};

/// Hubs that receive reports and serve updates, main first. The others are
/// mirrors for when a domain is blocked or down; updates are signature-
/// checked whichever one answers.
const HUBS: &[&str] = &["https://duoray.dualizm.space", "https://duoray.pro", "https://duoray.it-dualizm.space"];

/// The hub that answered last; tried first next time.
static PREFERRED: AtomicUsize = AtomicUsize::new(0);

/// Hubs in the order to try them. `DUORAY_HUB_URL` replaces the list (tests).
pub fn hubs() -> Vec<String> {
    if let Ok(url) = std::env::var("DUORAY_HUB_URL") {
        return vec![url];
    }
    let first = PREFERRED.load(Ordering::Relaxed) % HUBS.len();
    (0..HUBS.len()).map(|i| HUBS[(first + i) % HUBS.len()].to_string()).collect()
}

/// Remembers a hub that worked.
pub fn hub_ok(url: &str) {
    if let Some(i) = HUBS.iter().position(|h| *h == url) {
        PREFERRED.store(i, Ordering::Relaxed);
    }
}

const LOG_LINES: usize = 300;
/// Automatic reports per run; a crash loop must not flood the hub.
const AUTO_LIMIT: usize = 20;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SENT_AUTO: AtomicUsize = AtomicUsize::new(0);
static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);
static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

#[derive(Default)]
struct State {
    log: VecDeque<String>,
    queue: Option<PathBuf>,
    install_id: String,
    /// Settings / server summary, refreshed by the app; also used by panics.
    context: Value,
    /// Signatures already reported in this run.
    seen: HashSet<String>,
}

/// Sets where queued reports live and installs the panic hook.
pub fn init(data_dir: &Path) {
    LazyLock::force(&STARTED);
    let queue = data_dir.join("reports");
    let _ = std::fs::create_dir_all(&queue);
    STATE.lock().unwrap().queue = Some(queue);

    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        if !ENABLED.load(Ordering::Relaxed) {
            return;
        }
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let place = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let trace = std::backtrace::Backtrace::force_capture();
        // Written synchronously: the process may be about to die.
        if let Some(report) = build("panic", &format!("{what} at {place}\n\n{trace}"), None) {
            let _ = enqueue(&report);
        }
    }));
}

/// Applies the consent setting.
pub fn configure(enabled: bool, install_id: &str) {
    ENABLED.store(enabled, Ordering::Relaxed);
    STATE.lock().unwrap().install_id = install_id.to_string();
}

pub fn set_context(context: Value) {
    STATE.lock().unwrap().context = context;
}

/// One line for the activity log sent with reports (sanitized right away).
pub fn log(line: impl AsRef<str>) {
    let line = format!("{} {}", chrono::Local::now().format("%H:%M:%S"), sanitize(line.as_ref()));
    let mut st = STATE.lock().unwrap();
    if st.log.len() == LOG_LINES {
        st.log.pop_front();
    }
    st.log.push_back(line);
}

/// Reports an error automatically, if the user agreed to it. The same error
/// is reported once per run.
pub fn auto(kind: &str, message: &str) {
    log(format!("{kind}: {}", first_line(message)));
    if !ENABLED.load(Ordering::Relaxed) || SENT_AUTO.load(Ordering::Relaxed) >= AUTO_LIMIT {
        return;
    }
    let sig = signature(kind, message);
    if !STATE.lock().unwrap().seen.insert(sig) {
        return;
    }
    SENT_AUTO.fetch_add(1, Ordering::Relaxed);
    if let Some(report) = build(kind, message, None)
        && enqueue(&report).is_ok()
    {
        flush();
    }
}

/// The report "Сообщить о проблеме" would send, for the preview.
pub fn manual_report(note: &str) -> Value {
    build("user_report", "", Some(note)).unwrap_or(Value::Null)
}

/// Sends a report now and returns its code (DR-XXXXXX). Blocking.
pub fn send_now(report: &Value) -> Result<String> {
    Ok(post(report)?)
}

/// Deletes reports still waiting to be sent (the user opted out).
pub fn drop_queue() {
    let Some(queue) = STATE.lock().unwrap().queue.clone() else { return };
    if let Ok(dir) = std::fs::read_dir(queue) {
        for e in dir.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Sends queued reports in the background; failures stay queued.
pub fn flush() {
    let Some(queue) = STATE.lock().unwrap().queue.clone() else { return };
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&queue)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
            .unwrap_or_default();
        files.sort();
        for path in files {
            let Ok(report) = std::fs::read(&path).map_err(drop).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(drop)) else {
                let _ = std::fs::remove_file(&path);
                continue;
            };
            match post(&report) {
                Ok(_) | Err(PostError::Rejected) => {
                    let _ = std::fs::remove_file(&path);
                }
                // Offline or rate-limited: try again later.
                Err(PostError::Later(_)) => break,
            }
        }
        BUSY.store(false, Ordering::Relaxed);
    });
}

fn enqueue(report: &Value) -> Result<()> {
    let Some(queue) = STATE.lock().ok().and_then(|s| s.queue.clone()) else { bail!("no queue") };
    // Keep the queue bounded even if the hub is unreachable for a long time.
    if std::fs::read_dir(&queue).map(|d| d.count()).unwrap_or(0) >= 50 {
        bail!("queue full");
    }
    let name = format!("{}-{:04x}.json", chrono::Local::now().format("%Y%m%d%H%M%S"), fastrand::u16(..));
    std::fs::write(queue.join(name), serde_json::to_vec(report)?).context("queue report")
}

enum PostError {
    /// The hub refused this report for good (bad request): drop it.
    Rejected,
    Later(anyhow::Error),
}

impl From<PostError> for anyhow::Error {
    fn from(e: PostError) -> Self {
        match e {
            PostError::Rejected => anyhow::anyhow!("сервер отклонил отчёт"),
            PostError::Later(e) => e,
        }
    }
}

/// Sends to the first hub that takes it; a refusal (bad report) is final.
fn post(report: &Value) -> Result<String, PostError> {
    let mut last = PostError::Later(anyhow::anyhow!("нет серверов"));
    for hub in hubs() {
        match post_to(&hub, report) {
            Ok(id) => {
                hub_ok(&hub);
                return Ok(id);
            }
            Err(PostError::Rejected) => return Err(PostError::Rejected),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn post_to(hub: &str, report: &Value) -> Result<String, PostError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(format!("{hub}/v1/report"))
        .header("User-Agent", crate::update::USER_AGENT)
        .send_json(report)
        .map_err(|e| PostError::Later(e.into()))?;
    match resp.status().as_u16() {
        201 => {
            let body: Value = resp.body_mut().read_json().map_err(|e| PostError::Later(e.into()))?;
            Ok(body["id"].as_str().unwrap_or_default().to_string())
        }
        429 => Err(PostError::Later(anyhow::anyhow!("слишком много отчётов, попробуйте позже"))),
        400 | 404 | 413 => Err(PostError::Rejected),
        s => Err(PostError::Later(anyhow::anyhow!("сервер ответил {s}"))),
    }
}

fn build(kind: &str, message: &str, note: Option<&str>) -> Option<Value> {
    let st = STATE.lock().ok()?;
    let message = sanitize(message);
    let mut report = json!({
        "kind": kind,
        "message": message,
        "signature": signature(kind, &message),
        "version": env!("CARGO_PKG_VERSION"),
        "build": crate::update::BUILD,
        "os": os_label(),
        "arch": std::env::consts::ARCH,
        "install_id": st.install_id,
        "uptime_s": STARTED.elapsed().as_secs(),
        "context": st.context,
        "log": st.log.iter().cloned().collect::<Vec<_>>().join("\n"),
    });
    if let Some(note) = note {
        report["note"] = json!(sanitize(note));
    }
    // Where the tunnel and xray say what went wrong; only for those errors.
    if WITH_LOGS.contains(&kind) {
        let data = st.queue.as_deref().and_then(Path::parent);
        if let Some(text) = data.and_then(|d| tail_file(&d.join("run").join("xray.log"), 40)) {
            report["xray_log"] = json!(sanitize(&text));
        }
        if let Some(text) = helper_log(150) {
            report["helper_log"] = json!(sanitize(&text));
        }
    }
    Some(report)
}

/// Reports that carry the xray and helper logs.
const WITH_LOGS: &[&str] = &["user_report", "connect_failed", "tunnel_stopped", "helper_install"];

fn tail_file(path: &Path, lines: usize) -> Option<String> {
    let text = std::fs::read(path).ok()?;
    // Big logs: only the end matters.
    let text = String::from_utf8_lossy(&text[text.len().saturating_sub(256 * 1024)..]).into_owned();
    tail(&text, lines)
}

fn tail(text: &str, lines: usize) -> Option<String> {
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = all[all.len().saturating_sub(lines)..].join("\n");
    (!tail.is_empty()).then_some(tail)
}

/// The end of the TUN helper's log: per-app decisions, tunnel errors.
fn helper_log(lines: usize) -> Option<String> {
    if cfg!(windows) {
        let dir = PathBuf::from(std::env::var_os("ProgramData")?).join("DUORAY");
        tail_file(&dir.join("helper.log"), lines)
    } else if cfg!(target_os = "macos") {
        tail_file(Path::new("/var/log/duoray-helper.log"), lines)
    } else {
        // Readable for users in the systemd-journal / adm group only; else nothing.
        let out = std::process::Command::new("journalctl")
            .args(["-u", "duoray-helper", "-n", &lines.to_string(), "-o", "short-iso", "--no-pager"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        tail(&String::from_utf8_lossy(&out.stdout), lines).filter(|t| !t.starts_with("-- No entries"))
    }
}

fn os_label() -> String {
    static LABEL: LazyLock<String> = LazyLock::new(|| {
        let d = duoray_core::device::Device::detect();
        format!("{} {}", d.os, d.os_version.unwrap_or_default()).trim().to_string()
    });
    LABEL.clone()
}

/// Groups reports of the same error: the kind plus the first line with
/// numbers blanked ("exit code 1" and "exit code 2" are one error).
fn signature(kind: &str, message: &str) -> String {
    static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    let line = DIGITS.replace_all(first_line(message), "#");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in kind.bytes().chain(*b"\n").chain(line.bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")[..12].to_string()
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// Removes everything that could identify the user, their servers or
/// subscriptions. Over-redacting is fine; leaking is not.
pub fn sanitize(text: &str) -> String {
    static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
        let rule = |re: &str, with| (Regex::new(re).unwrap(), with);
        vec![
            rule(r"(?i)\b[a-z][a-z0-9+.\-]{1,15}://\S+", "<url>"),
            rule(r"(?i)\b[\w.+\-]+@[\w\-]+(\.[\w\-]+)+", "<email>"),
            rule(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b", "<uuid>"),
            // IPv6: full form (4+ groups) or compressed with "::".
            rule(r"(?i)\[?\b(?:[0-9a-f]{1,4}:){4,7}[0-9a-f]{1,4}\b\]?(:\d+)?", "<ip>"),
            rule(r"(?i)\[?\b(?:[0-9a-f]{1,4}:)+:(?:[0-9a-f]{1,4}:?)*[0-9a-f]{0,4}\b\]?(:\d+)?", "<ip>"),
            rule(r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b", "<ip>"),
            // Windows / Unix home directories.
            rule(r"(?i)\b[a-z]:\\Users\\[^\\\s]+", r"C:\Users\<user>"),
            rule(r"/home/[^/\s]+", "/home/<user>"),
            rule(r"/Users/[^/\s]+", "/Users/<user>"),
            // Host names; file names with known extensions are kept.
            rule(
                r"(?i)\b(?:[a-z0-9](?:[a-z0-9\-]{0,61}[a-z0-9])?\.)+(?:[a-z][a-z0-9\-]{1,62})\b",
                "<host>",
            ),
        ]
    });
    static FILE_EXT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\.(json|exe|dll|dat|log|toml|rs|service|sock|txt|png|svg|so|sh|conf|ya?ml|py|slint|pem|plist|app)$")
            .unwrap()
    });
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/_=]{16,}").unwrap());

    let mut out = text.to_string();
    for (re, with) in RULES.iter() {
        out = if *with == "<host>" {
            re.replace_all(&out, |c: &regex::Captures| {
                let m = &c[0];
                if FILE_EXT.is_match(m) { m.to_string() } else { "<host>".to_string() }
            })
            .into_owned()
        } else {
            re.replace_all(&out, *with).into_owned()
        };
    }
    // Keys, short ids, tokens: long runs of base64/hex that contain a digit.
    TOKEN
        .replace_all(&out, |c: &regex::Captures| {
            let m = &c[0];
            if m.bytes().any(|b| b.is_ascii_digit()) { "<secret>".to_string() } else { m.to_string() }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizer_hides_what_identifies_users() {
        let cases = [
            ("fetch https://panel.example.com/sub/AbC123xyz?token=1 failed", "fetch <url> failed"),
            ("vless://u@1.2.3.4:443?sni=x#n broke", "<url> broke"),
            ("dial tcp 198.51.100.7:443: i/o timeout", "dial tcp <ip>: i/o timeout"),
            ("lookup de.example.org: no such host", "lookup <host>: no such host"),
            ("id 0b5e1c9a-2f1d-4a7e-9c3b-1e2d3f4a5b6c rejected", "id <uuid> rejected"),
            ("ipv6 [2a01:4f8:c0c:1234::1]:443 down", "ipv6 <ip> down"),
            ("pbk Zm9vYmFyYmF6cXV4MTIz sid 6ba85179e30d4fc2", "pbk <secret> sid <secret>"),
            ("write C:\\Users\\Ivan\\AppData\\x.json", "write C:\\Users\\<user>\\AppData\\x.json"),
            ("open /home/ivan/.local/share/duoray/store.json", "open /home/<user>/.local/share/duoray/store.json"),
            ("mail me at a.b@mail.ru", "mail me at <email>"),
        ];
        for (input, want) in cases {
            assert_eq!(sanitize(input), want, "{input}");
        }
        // Ordinary diagnostics survive.
        let keep = "12:00:01 xray остановился (exit status: 1). geoip.dat не найден, flow xtls-rprx-vision";
        assert_eq!(sanitize(keep), keep);
    }

    #[test]
    fn mirrors_rotate_to_the_last_good_hub() {
        assert_eq!(hubs()[0], HUBS[0]);
        hub_ok(HUBS[2]);
        assert_eq!(hubs(), [HUBS[2], HUBS[0], HUBS[1]]);
        hub_ok("https://unknown.example");
        assert_eq!(hubs()[0], HUBS[2]);
        hub_ok(HUBS[0]);
    }

    #[test]
    fn signatures_ignore_numbers() {
        assert_eq!(signature("x", "exit code 1\nmore"), signature("x", "exit code 23"));
        assert_ne!(signature("x", "exit code 1"), signature("y", "exit code 1"));
    }
}

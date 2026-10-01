//! Privileged helper: the only DUORAY component that runs with admin rights.
//!
//! Installed once (LaunchDaemon on macOS, systemd unit on Linux, a service on
//! Windows), it brings the TUN up on request of the unprivileged GUI. A session
//! is bound to the client connection: when the GUI disconnects or dies, the TUN
//! goes down and routes/DNS are restored. Stopping the service does the same.

#![cfg_attr(windows, allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, bail};
use duoray_core::helper_proto::{PROTOCOL, Request, Response, TunRequest};
use duotun::socks5::Socks5;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

fn main() -> Result<()> {
    let logs = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,duotun=info".into()),
        )
        .with_ansi(false);

    #[cfg(windows)]
    if std::env::args().any(|a| a == "--service") {
        // A service has no console: log to %ProgramData%\DUORAY\helper.log.
        match win::log_file() {
            Some(file) => logs.with_writer(std::sync::Mutex::new(file)).init(),
            None => logs.init(),
        }
        return win::run_service();
    }
    logs.init();

    // Foreground (launchd/systemd, or `--console` on Windows for debugging).
    let (tx, rx) = watch::channel(false);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = tx.send(true);
        });
        serve_forever(rx).await
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Accepts clients until `shutdown` flips, then waits for every session to
/// restore the network.
async fn serve_forever(shutdown: watch::Receiver<bool>) -> Result<()> {
    // A previous instance may have died with the TUN up.
    duotun::sys::recover();
    let busy = Arc::new(AtomicBool::new(false));
    let mut sessions = tokio::task::JoinSet::new();
    let result = accept_loop(&mut sessions, busy, shutdown).await;
    while sessions.join_next().await.is_some() {}
    info!("helper stopped");
    result
}

#[cfg(unix)]
async fn accept_loop(
    sessions: &mut tokio::task::JoinSet<()>,
    busy: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    use duoray_core::helper_proto::SOCKET_PATH;
    use tokio::net::UnixListener;

    let allowed_uid = allowed_uid_arg()?;
    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH).with_context(|| format!("binding {SOCKET_PATH}"))?;
    std::os::unix::fs::chown(SOCKET_PATH, Some(allowed_uid), None)?;
    std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o600))?;
    info!(
        "duoray-helper {} listening on {SOCKET_PATH} for uid {allowed_uid}",
        env!("CARGO_PKG_VERSION")
    );

    loop {
        let (stream, _) = tokio::select! {
            r = listener.accept() => r?,
            _ = shutdown.changed() => break,
        };
        let uid = stream.peer_cred().map(|c| c.uid()).unwrap_or(u32::MAX);
        if uid != allowed_uid && uid != 0 {
            warn!("rejected connection from uid {uid}");
            continue;
        }
        let (busy, shutdown) = (busy.clone(), shutdown.clone());
        sessions.spawn(async move {
            if let Err(e) = serve(stream, busy, shutdown).await {
                warn!("client session ended: {e:#}");
            }
        });
    }
    let _ = std::fs::remove_file(SOCKET_PATH);
    Ok(())
}

#[cfg(windows)]
async fn accept_loop(
    sessions: &mut tokio::task::JoinSet<()>,
    busy: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    use duoray_core::helper_proto::PIPE_NAME;

    let sd = win::PipeSecurity::new()?;
    let mut server = sd.create_pipe(PIPE_NAME, true)?;
    info!("duoray-helper {} listening on {PIPE_NAME}", env!("CARGO_PKG_VERSION"));
    loop {
        tokio::select! {
            r = server.connect() => r?,
            _ = shutdown.changed() => break,
        }
        // Hand the connected instance over and open the next one for new clients.
        let client = std::mem::replace(&mut server, sd.create_pipe(PIPE_NAME, false)?);
        let (busy, shutdown) = (busy.clone(), shutdown.clone());
        sessions.spawn(async move {
            if let Err(e) = serve(client, busy, shutdown).await {
                warn!("client session ended: {e:#}");
            }
        });
    }
    Ok(())
}

#[cfg(unix)]
fn allowed_uid_arg() -> Result<u32> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--allowed-uid" {
            return args.next().context("--allowed-uid needs a value")?.parse().context("bad uid");
        }
    }
    bail!("usage: duoray-helper --allowed-uid <uid>")
}

struct Session {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
}

/// Clears the busy flag however the session ends.
struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

enum Event {
    Line(Option<String>),
    TunExited(Result<Result<()>, tokio::task::JoinError>),
    Shutdown,
}

/// Resolves when the running session's TUN task ends; never if there is none.
async fn session_ended(session: &mut Option<(Session, BusyGuard)>) -> Result<Result<()>, tokio::task::JoinError> {
    match session {
        Some((s, _)) => (&mut s.task).await,
        None => std::future::pending().await,
    }
}

async fn serve<S>(stream: S, busy: Arc<AtomicBool>, mut shutdown: watch::Receiver<bool>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    let mut session: Option<(Session, BusyGuard)> = None;
    // Why the TUN went down on its own; reported once on the next Status.
    let mut died: Option<String> = None;

    loop {
        let event = tokio::select! {
            line = lines.next_line() => Event::Line(line?),
            result = session_ended(&mut session) => Event::TunExited(result),
            _ = shutdown.changed() => Event::Shutdown,
        };
        let line = match event {
            Event::Line(line) => line,
            Event::Shutdown => break,
            Event::TunExited(result) => {
                session = None;
                died = Some(match result {
                    Ok(Ok(())) => "TUN stopped".to_string(),
                    Ok(Err(e)) => format!("{e:#}"),
                    Err(e) => format!("TUN task panicked: {e}"),
                });
                warn!("TUN exited: {}", died.as_deref().unwrap_or_default());
                continue;
            }
        };
        let Some(line) = line else { break };
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(&mut w, &Response::Error { message: format!("bad request: {e}") }).await?;
                continue;
            }
        };
        let resp = match req {
            Request::Hello => Response::Hello {
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol: PROTOCOL,
            },
            Request::Status => Response::Status {
                running: session.is_some(),
                error: died.take(),
            },
            Request::Start(_) if session.is_some() => Response::Error { message: "already started".into() },
            Request::Start(req) => {
                if busy.swap(true, Ordering::SeqCst) {
                    Response::Error { message: "another DUORAY session is active".into() }
                } else {
                    let guard = BusyGuard(busy.clone());
                    died = None;
                    match start(req).await {
                        Ok((s, tun)) => {
                            session = Some((s, guard));
                            Response::Started { tun }
                        }
                        Err(e) => Response::Error { message: format!("{e:#}") },
                    }
                }
            }
            Request::Stop => {
                if let Some((s, _guard)) = session.take() {
                    stop(s).await;
                }
                Response::Stopped
            }
        };
        send(&mut w, &resp).await?;
    }

    // Client went away or the service is stopping: never leave the TUN behind.
    if let Some((s, _guard)) = session.take() {
        info!("client gone, stopping TUN");
        stop(s).await;
    }
    Ok(())
}

async fn start(req: TunRequest) -> Result<(Session, String)> {
    validate(&req)?;
    let mut cfg = duotun::Config {
        socks: Socks5 {
            server: req.socks,
            auth: Some((req.user, req.pass)),
        },
        bypass: req.bypass,
        ..Default::default()
    };
    // Wintun adapters are addressed by name; give ours a recognizable one.
    if cfg!(windows) {
        cfg.tun_name = Some("DUORAY".into());
    }
    if !req.ipv6 {
        cfg.v6 = None;
    }
    info!(socks = %cfg.socks.server, bypass = ?cfg.bypass, "starting TUN");

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let (ready_tx, ready_rx) = oneshot::channel::<String>();
    let mut task = tokio::spawn(duotun::run_notify(
        cfg,
        async {
            let _ = stop_rx.await;
        },
        move |tun| {
            let _ = ready_tx.send(tun);
        },
    ));

    tokio::select! {
        tun = ready_rx => match tun {
            Ok(tun) => {
                info!("TUN {tun} up");
                Ok((Session { stop: stop_tx, task }, tun))
            }
            // Sender dropped: run_notify failed before the TUN came up.
            Err(_) => match (&mut task).await {
                Ok(Err(e)) => Err(e),
                Ok(Ok(())) => bail!("TUN exited before coming up"),
                Err(e) => bail!("TUN task panicked: {e}"),
            },
        },
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            let _ = stop_tx.send(());
            let _ = task.await;
            bail!("timed out bringing the TUN up")
        }
    }
}

async fn stop(s: Session) {
    let _ = s.stop.send(());
    match s.task.await {
        Ok(Ok(())) => info!("TUN stopped"),
        Ok(Err(e)) => warn!("TUN stopped with error: {e:#}"),
        Err(e) => warn!("TUN task panicked: {e}"),
    }
}

fn validate(req: &TunRequest) -> Result<()> {
    // Only ever tunnel into a proxy on this machine.
    if !req.socks.ip().is_loopback() {
        bail!("SOCKS address must be loopback, got {}", req.socks);
    }
    if req.user.is_empty() || req.pass.is_empty() {
        bail!("SOCKS credentials are required");
    }
    if req.bypass.len() > 256 {
        bail!("too many bypass addresses");
    }
    Ok(())
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, resp: &Response) -> Result<()> {
    let mut line = serde_json::to_vec(resp)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(windows)]
mod win {
    //! Windows service plumbing and the pipe's access control.

    use std::ffi::OsString;
    use std::time::Duration;

    use anyhow::{Result, bail};
    use duoray_core::helper_proto::SERVICE_NAME;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::sync::watch;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::{define_windows_service, service_dispatcher};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    define_windows_service!(ffi_service_main, service_main);

    /// Appends to %ProgramData%\DUORAY\helper.log, restarting it past 1 MB.
    pub fn log_file() -> Option<std::fs::File> {
        let dir = std::path::PathBuf::from(std::env::var_os("ProgramData")?).join("DUORAY");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("helper.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
            let _ = std::fs::rename(&path, dir.join("helper.old.log"));
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
    }

    pub fn run_service() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
        Ok(())
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_as_service() {
            tracing::error!("service failed: {e:#}");
        }
    }

    fn run_as_service() -> Result<()> {
        let (tx, rx) = watch::channel(false);
        let handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = tx.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status = service_control_handler::register(SERVICE_NAME, handler)?;
        let report = |state, accept| {
            let _ = status.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: accept,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::from_secs(10),
                process_id: None,
            });
        };
        report(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN);
        let result = tokio::runtime::Runtime::new()?.block_on(super::serve_forever(rx));
        report(ServiceState::Stopped, ServiceControlAccept::empty());
        result
    }

    /// SYSTEM and Administrators get full access, signed-in users may read and
    /// write (the GUI runs as a normal user). Requests are validated anyway.
    pub struct PipeSecurity {
        descriptor: PSECURITY_DESCRIPTOR,
    }

    unsafe impl Send for PipeSecurity {}

    impl PipeSecurity {
        pub fn new() -> Result<Self> {
            let sddl: Vec<u16> = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)\0".encode_utf16().collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: valid NUL-terminated wide string; the out pointer is ours.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                bail!("cannot build the pipe security descriptor");
            }
            Ok(Self { descriptor })
        }

        pub fn create_pipe(&self, name: &str, first: bool) -> Result<NamedPipeServer> {
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor,
                bInheritHandle: 0,
            };
            // SAFETY: `attrs` outlives the call; the descriptor lives in `self`.
            let pipe = unsafe {
                ServerOptions::new()
                    .first_pipe_instance(first)
                    .create_with_security_attributes_raw(name, &mut attrs as *mut _ as *mut _)
            }?;
            Ok(pipe)
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

//! One GUI owns a data directory, including its xray PID and config files.
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Keep the returned handle alive until connection teardown finishes. Never
/// unlink the file: another process could then lock a different inode.
pub fn acquire(dir: &Path, wait: bool) -> Result<Option<File>> {
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(dir.join("instance.lock")).context("открытие блокировки DUORAY")?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) if wait && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e).context("блокировка DUORAY"),
        }
    }
}

/// The running instance listens on loopback for "show" from later launches;
/// the port goes to `instance.port` next to the lock.
pub fn serve(dir: &Path, on_show: impl Fn() + Send + 'static) {
    use std::io::Read;
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else { return };
    let Ok(addr) = listener.local_addr() else { return };
    if std::fs::write(dir.join("instance.port"), addr.port().to_string()).is_err() {
        return;
    }
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut buf = [0; 16];
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let n = (&stream).read(&mut buf).unwrap_or(0);
            if buf[..n].starts_with(b"show") {
                on_show();
            }
        }
    });
}

/// Asks the running instance to show its window; false if it did not answer
/// (an older version, or it is shutting down).
pub fn signal(dir: &Path) -> bool {
    use std::io::Write;
    let Some(port) = std::fs::read_to_string(dir.join("instance.port")).ok().and_then(|p| p.trim().parse::<u16>().ok())
    else {
        return false;
    };
    // Windows lets only the foreground process raise a window; we are it.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX);
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).and_then(|mut s| s.write_all(b"show\n")).is_ok()
}

pub fn already_running() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_OK, MB_ICONINFORMATION, MessageBoxW};
        let text: Vec<u16> = "DUORAY уже запущен. Используйте открытое окно приложения.\0".encode_utf16().collect();
        let title: Vec<u16> = "DUORAY\0".encode_utf16().collect();
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONINFORMATION); }
    }
    #[cfg(not(windows))]
    eprintln!("DUORAY уже запущен.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_second_owner_and_releases_on_close() {
        let dir = std::env::temp_dir().join(format!("duoray-instance-{}-{}", std::process::id(), fastrand::u64(..)));
        let first = acquire(&dir, false).unwrap().unwrap();
        assert!(acquire(&dir, false).unwrap().is_none());
        let waiting_dir = dir.clone();
        let replacement = std::thread::spawn(move || acquire(&waiting_dir, true).unwrap().unwrap());
        drop(first);
        let replacement = replacement.join().unwrap();
        assert!(acquire(&dir, false).unwrap().is_none());
        drop(replacement);
        let next = acquire(&dir, false).unwrap().unwrap();
        drop(next);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

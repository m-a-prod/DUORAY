//! SIGTERM, SIGINT and SIGHUP (logout, shutdown, `kill`, Ctrl+C) end the app
//! like "Выйти" in the tray, so the quit path disconnects and puts the system
//! proxy back instead of leaving it pointed at a dead port.

use std::sync::atomic::{AtomicI32, Ordering};

/// Write end of the pipe the handler wakes the waiting thread through.
static PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    let fd = PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write(2) is async-signal-safe; the buffer is a static byte.
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

pub fn quit_on_termination() {
    let mut fds = [0; 2];
    // SAFETY: fds has room for the two descriptors pipe(2) returns.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    for fd in fds {
        // SAFETY: fd was just opened; xray and other children must not inherit it.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    PIPE.store(fds[1], Ordering::Relaxed);
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction with a handler that only calls write(2).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
    std::thread::spawn(move || {
        let mut byte = 0u8;
        loop {
            // SAFETY: reads one byte into a local.
            let n = unsafe { libc::read(fds[0], (&raw mut byte).cast(), 1) };
            if n == 1 {
                break;
            }
            if n == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return;
            }
        }
        crate::diag::log("signal: quitting");
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });
}

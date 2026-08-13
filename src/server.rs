use io_uring::{IoUring, opcode, types};
use std::net::TcpListener;
use std::os::unix::io::AsRawFd;
use std::time::Duration;

const ACCEPT_TOKEN: u64 = 1;
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Backoff between retries when a non-transient accept error occurs (e.g. EMFILE).
/// Without this, a sustained error causes a hot spin that floods stderr and pegs the CPU.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

pub fn run(bind_addr: &str, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind((bind_addr, port))?;

    // io_uring is unavailable in plenty of real deployments: seccomp profiles
    // in hardened containers, Kubernetes PSPs, kernels older than 5.1, or
    // io_uring_disabled=2. For a loop that only accepts and closes, blocking
    // accept4() is an adequate substitute, so degrade instead of refusing to
    // start. Errors that are not about availability still propagate.
    let ring = match IoUring::new(256) {
        Ok(ring) => Some(ring),
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOSYS | libc::EPERM | libc::EACCES | libc::EOPNOTSUPP)
            ) =>
        {
            eprintln!("warn: io_uring unavailable ({e})  -  falling back to blocking accept");
            None
        }
        Err(e) => return Err(e),
    };

    let pid = unsafe { libc::getpid() };
    let mode = if ring.is_some() {
        "io_uring accept/drop loop"
    } else {
        "blocking accept/drop loop (io_uring unavailable)"
    };
    eprintln!(
        "wire-probe {VERSION}  -  L4 TCP telemetry server
  pid:     {pid}
  listen:  {bind_addr}:{port}
  mode:    {mode}
  author:  Matheus Santos <vorj.dux@gmail.com>"
    );
    if bind_addr == "0.0.0.0" || bind_addr == "::" {
        eprintln!(
            "warn: listening on all interfaces  -  restrict with --bind <private-ip> \
             or enforce access via firewall/NSG rules"
        );
    }

    match ring {
        Some(ring) => run_io_uring(&listener, ring),
        None => run_blocking(&listener),
    }
}

/// Blocking fallback. `TcpListener::accept` uses accept4(SOCK_CLOEXEC) on
/// Linux, so accepted fds do not leak into children here either. The stream is
/// dropped immediately, which closes it.
fn run_blocking(listener: &TcpListener) -> std::io::Result<()> {
    loop {
        match listener.accept() {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                // Same reasoning as the io_uring path: back off so a sustained
                // failure (EMFILE) neither hot-spins nor floods stderr.
                eprintln!("warn: accept error: {e}");
                std::thread::sleep(ACCEPT_ERROR_BACKOFF);
            }
        }
    }
}

fn run_io_uring(listener: &TcpListener, mut ring: IoUring) -> std::io::Result<()> {
    let listener_fd = listener.as_raw_fd();

    push_accept(&mut ring, listener_fd)?;
    ring.submit()?;

    loop {
        ring.submit_and_wait(1)?;

        let mut rearm = false;
        {
            let mut cq = ring.completion();
            for cqe in &mut cq {
                if cqe.user_data() == ACCEPT_TOKEN {
                    let fd = cqe.result();
                    if fd >= 0 {
                        if unsafe { libc::close(fd) } != 0 {
                            let err = std::io::Error::last_os_error();
                            eprintln!("warn: close(fd={fd}) failed: {err}");
                        }
                    } else {
                        // Negative result means the accept syscall failed.
                        let errno = -fd;
                        if errno != libc::EAGAIN && errno != libc::EWOULDBLOCK {
                            // Non-transient error (e.g. EMFILE  -  fd table exhausted).
                            // Log once and sleep so a sustained failure neither hot-spins
                            // the CPU nor floods stderr with one line per connection.
                            eprintln!(
                                "warn: accept error: {}",
                                std::io::Error::from_raw_os_error(errno)
                            );
                            std::thread::sleep(ACCEPT_ERROR_BACKOFF);
                        }
                    }
                    rearm = true;
                }
            }
            // CompletionQueue dropped here, releasing the borrow on ring
        }

        if rearm {
            push_accept(&mut ring, listener_fd)?;
            ring.submit()?;
        }
    }
}

fn push_accept(ring: &mut IoUring, fd: i32) -> std::io::Result<()> {
    // SOCK_CLOEXEC: prevent accepted fds from leaking into any child process.
    // addr/addrlen are null: client address is not needed since we drop immediately.
    let sqe = opcode::Accept::new(types::Fd(fd), std::ptr::null_mut(), std::ptr::null_mut())
        .flags(libc::SOCK_CLOEXEC)
        .build()
        .user_data(ACCEPT_TOKEN);

    unsafe {
        ring.submission()
            .push(&sqe)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::WouldBlock, "SQ full"))?;
    }
    Ok(())
}

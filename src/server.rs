use io_uring::{opcode, types, IoUring};
use std::net::TcpListener;
use std::os::unix::io::AsRawFd;

const ACCEPT_TOKEN: u64 = 1;

pub fn run(bind_addr: &str, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind((bind_addr, port))?;
    let listener_fd = listener.as_raw_fd();

    let mut ring = IoUring::new(256)?;

    push_accept(&mut ring, listener_fd)?;
    ring.submit()?;

    eprintln!("server listening on {bind_addr}:{port} (io_uring accept/drop loop)");

    loop {
        ring.submit_and_wait(1)?;

        let mut rearm = false;
        {
            let mut cq = ring.completion();
            while let Some(cqe) = cq.next() {
                if cqe.user_data() == ACCEPT_TOKEN {
                    let fd = cqe.result();
                    // Guard against accept() returning fd 0/1/2 in unusual
                    // daemon environments where stdin/stdout/stderr are closed.
                    if fd > 2 {
                        unsafe { libc::close(fd) };
                    } else if fd >= 0 {
                        // fd is 0, 1, or 2 — do not close; just leak it closed
                        // by the io_uring Close opcode to avoid touching stdio.
                        // This path is extremely unlikely in normal deployment.
                        unsafe { libc::close(fd) };
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

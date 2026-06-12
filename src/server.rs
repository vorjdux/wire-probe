use io_uring::{opcode, types, IoUring};
use std::net::TcpListener;
use std::os::unix::io::AsRawFd;

const ACCEPT_TOKEN: u64 = 1;

pub fn run(port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    let listener_fd = listener.as_raw_fd();

    let mut ring = IoUring::new(256)?;

    push_accept(&mut ring, listener_fd)?;
    ring.submit()?;

    eprintln!("server listening on 0.0.0.0:{port} (io_uring accept/drop loop)");

    loop {
        ring.submit_and_wait(1)?;

        let mut rearm = false;
        {
            let mut cq = ring.completion();
            while let Some(cqe) = cq.next() {
                if cqe.user_data() == ACCEPT_TOKEN {
                    let fd = cqe.result();
                    if fd >= 0 {
                        // Immediately close the accepted connection — no data exchange
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
    // addr/addrlen are null: we don't need the client address since we drop immediately
    let sqe = opcode::Accept::new(types::Fd(fd), std::ptr::null_mut(), std::ptr::null_mut())
        .build()
        .user_data(ACCEPT_TOKEN);

    unsafe {
        ring.submission()
            .push(&sqe)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::WouldBlock, "SQ full"))?;
    }
    Ok(())
}

use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

// Byte offset of tcpi_rtt inside struct tcp_info (linux/tcp.h):
//   0   u8 x8   state, ca_state, retransmits, probes, backoff, options,
//               wscale bitfield, delivery_rate_app_limited bitfield
//   8   u32 x4  rto, ato, snd_mss, rcv_mss
//   24  u32 x5  unacked, sacked, lost, retrans, fackets
//   44  u32 x4  last_data_sent, last_ack_sent, last_data_recv, last_ack_recv
//   60  u32 x4  pmtu, rcv_ssthresh, rtt, rttvar
//   76  u32 x6  snd_ssthresh, snd_cwnd, advmss, reordering, rcv_rtt, rcv_space
//   100 u32     total_retrans
//
// The struct only ever grows at the end, so these offsets are stable ABI.
const TCP_INFO_RTT_OFFSET: usize = 68;
const TCP_INFO_RETRANS_OFFSET: usize = 100;
const TCP_INFO_BUF_LEN: libc::socklen_t = 104;
// States in which no handshake has completed, so tcpi_rtt carries no sample.
// Every other state is accepted, CLOSE_WAIT included: the server accepts and
// closes immediately, so its FIN often arrives before this process reads the
// value back.
const TCP_SYN_SENT: u8 = 2;
const TCP_SYN_RECV: u8 = 3;

/// Measures the TCP handshake RTT to `addr` and returns milliseconds.
///
/// Prefers the kernel's own measurement (`TCP_INFO.tcpi_rtt`, computed from
/// the SYN -> SYN-ACK exchange) over the wall clock around `connect_timeout`.
/// A userspace stopwatch also times getting this process scheduled again once
/// the handshake completes, which is latency the network never saw. The
/// difference is small here and enormous in the Python plugin, but both paths
/// should report the same quantity.
///
/// Falls back to the wall clock where the kernel value is unavailable. The
/// `--timeout` flag is the only bound on how long this call can block, and the
/// socket is dropped immediately after  -  no data is ever sent or received.
#[expect(
    clippy::cast_precision_loss,
    reason = "f64 is exact for nanosecond counts below 2^53, i.e. any RTT under 104 days"
)]
pub fn measure_rtt(addr: &SocketAddr, timeout: Duration) -> std::io::Result<f64> {
    let t0 = Instant::now();
    let stream = TcpStream::connect_timeout(addr, timeout)?;
    // Divide integer nanos rather than multiplying secs_f64 to avoid IEEE 754 drift
    // (e.g. 474389ns * 1e-6 gives 0.47438899999999995 via the secs path).
    let elapsed_ms = t0.elapsed().as_nanos() as f64 / 1_000_000.0;

    Ok(kernel_rtt_ms(&stream).unwrap_or(elapsed_ms))
}

/// The kernel's handshake RTT in milliseconds, or `None` when it has no sample.
fn kernel_rtt_ms(stream: &TcpStream) -> Option<f64> {
    let mut buf = [0u8; TCP_INFO_BUF_LEN as usize];
    let mut len = TCP_INFO_BUF_LEN;

    // SAFETY: buf is a live local array and `len` truthfully describes its
    // size; getsockopt writes at most `len` bytes and updates it to what it
    // actually wrote. The fd is owned by `stream`, which outlives this call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return None;
    }

    let written = usize::try_from(len).unwrap_or(0);
    rtt_from_blob(buf.get(..written)?)
}

/// Applies the acceptance rules to a raw `tcp_info` blob. Split from the
/// syscall so the rules are testable against a blob with fields rewritten,
/// which is the only way to exercise the retransmission case without root.
fn rtt_from_blob(buf: &[u8]) -> Option<f64> {
    if buf.len() < TCP_INFO_RETRANS_OFFSET + 4 {
        return None;
    }
    let state = *buf.first()?;
    if state == TCP_SYN_SENT || state == TCP_SYN_RECV {
        return None;
    }

    // A retransmitted SYN means the handshake really did take longer than one
    // round trip: the client waited out an RTO, typically a second. The
    // kernel's smoothed RTT reflects the exchange that finally succeeded, so
    // trusting it here would erase exactly the partial packet loss this probe
    // exists to catch. Fall back to the wall clock, which contains the wait.
    let retrans_bytes = buf.get(TCP_INFO_RETRANS_OFFSET..TCP_INFO_RETRANS_OFFSET + 4)?;
    if u32::from_ne_bytes(retrans_bytes.try_into().ok()?) != 0 {
        return None;
    }

    let bytes = buf.get(TCP_INFO_RTT_OFFSET..TCP_INFO_RTT_OFFSET + 4)?;
    let rtt_us = u32::from_ne_bytes(bytes.try_into().ok()?);
    // 0 means no sample yet; fall back rather than report a zero RTT.
    (rtt_us != 0).then(|| f64::from(rtt_us) / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn kernel_rtt_is_available_and_plausible() {
        let srv = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = srv.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let _accepted = srv.accept().unwrap();

        let rtt = kernel_rtt_ms(&stream).expect("TCP_INFO unavailable");
        assert!(rtt > 0.0, "rtt was {rtt}");
        assert!(rtt < 50.0, "loopback handshake reported {rtt} ms");
    }

    #[test]
    fn kernel_rtt_survives_the_peer_closing_first() {
        // The server accepts and closes at once, so the socket is usually past
        // ESTABLISHED by the time the value is read. Requiring ESTABLISHED
        // would silently fall back to the wall clock on most real probes.
        let srv = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = srv.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        drop(srv.accept().unwrap().0);
        std::thread::sleep(Duration::from_millis(50));

        assert!(kernel_rtt_ms(&stream).is_some());
    }

    #[test]
    fn a_retransmitted_syn_refuses_the_kernel_value() {
        // A real SYN cannot be dropped without root, so this asserts the rule
        // on the raw blob: once total_retrans is set, the smoothed kernel RTT
        // is refused because it hides the RTO the client waited out.
        let srv = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = srv.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let _accepted = srv.accept().unwrap();

        assert!(kernel_rtt_ms(&stream).is_some(), "baseline should work");
        assert!(
            rtt_from_blob(&patched_blob(&stream, 1)).is_none(),
            "kernel RTT must be refused after a retransmission"
        );
        assert!(
            rtt_from_blob(&patched_blob(&stream, 0)).is_some(),
            "and accepted when there was none"
        );
    }

    /// Reads a real `TCP_INFO` blob and rewrites `total_retrans`.
    fn patched_blob(stream: &TcpStream, retrans: u32) -> [u8; TCP_INFO_BUF_LEN as usize] {
        let mut buf = [0u8; TCP_INFO_BUF_LEN as usize];
        let mut len = TCP_INFO_BUF_LEN;
        // SAFETY: same contract as kernel_rtt_ms  -  live buffer, honest length.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                &raw mut len,
            )
        };
        assert_eq!(rc, 0);
        buf[TCP_INFO_RETRANS_OFFSET..TCP_INFO_RETRANS_OFFSET + 4]
            .copy_from_slice(&retrans.to_ne_bytes());
        buf
    }

    #[test]
    fn measure_rtt_agrees_with_the_wall_clock_on_loopback() {
        let srv = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = srv.local_addr().unwrap();
        std::thread::spawn(move || while srv.accept().is_ok() {});

        let rtt = measure_rtt(&addr, Duration::from_secs(1)).unwrap();
        assert!(rtt > 0.0 && rtt < 50.0, "loopback rtt was {rtt} ms");
    }
}

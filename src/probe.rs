use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Measures the TCP handshake RTT to `target` and returns milliseconds.
///
/// SO_RCVTIMEO / SO_SNDTIMEO are set after connect as safety guards for any
/// subsequent I/O (none happens here, but the pattern follows the ADR intent).
pub fn measure_rtt(target: &str, timeout: Duration) -> std::io::Result<f64> {
    let addr = target
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address resolved"))?;

    let t0 = Instant::now();
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    // Divide integer nanos rather than multiplying secs_f64 to avoid IEEE 754 drift
    // (e.g. 474389ns * 1e-6 gives 0.47438899999999995 via the secs path).
    let rtt_ms = t0.elapsed().as_nanos() as f64 / 1_000_000.0;

    // Aggressive OS-level timeouts: block no longer than `timeout` on any future I/O.
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // Drop sends RST/FIN, completing the measurement cycle.
    drop(stream);
    Ok(rtt_ms)
}

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// Measures the TCP handshake RTT to `addr` and returns milliseconds.
///
/// Timing is entirely `connect_timeout` + `Instant`; the `--timeout` flag
/// is the only bound on how long this call can block. The socket is dropped
/// immediately after the handshake  -  no data is ever sent or received.
#[allow(clippy::cast_precision_loss)]
pub fn measure_rtt(addr: &SocketAddr, timeout: Duration) -> std::io::Result<f64> {
    let t0 = Instant::now();
    let _stream = TcpStream::connect_timeout(addr, timeout)?;
    // Divide integer nanos rather than multiplying secs_f64 to avoid IEEE 754 drift
    // (e.g. 474389ns * 1e-6 gives 0.47438899999999995 via the secs path).
    Ok(t0.elapsed().as_nanos() as f64 / 1_000_000.0)
}

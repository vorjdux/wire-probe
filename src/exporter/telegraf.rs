use std::io;
use std::net::UdpSocket;

/// Bytes reserved past the prefix for `rtt_ms=`, the value, `,success=Ni`,
/// the space, and a 19-digit nanosecond timestamp.
const VALUE_HEADROOM: usize = 64;

/// Formats and ships Influx Line Protocol measurements to a Telegraf
/// `[[inputs.socket_listener]]` UDP endpoint.
///
/// The static prefix (`tcp_latency,target=<name>,az=<az> `) is built once at
/// construction; only the float value and timestamp are formatted on each
/// send, with zero heap allocation on the hot path.
///
/// Every probe emits a point, successful or not:
///   success:  `tcp_latency,target=x,az=y rtt_ms=4.12,success=1i <ts>`
///   failure:  `tcp_latency,target=x,az=y success=0i <ts>`
///
/// A failed probe must produce a value rather than a gap: over fire-and-forget
/// UDP a missing point is indistinguishable from a lost datagram, so alerting
/// on absence cannot tell "target down" from "exporter down".
pub struct TelegrafExporter {
    socket: UdpSocket,
    prefix: Vec<u8>,
    buf: Vec<u8>,
}

impl TelegrafExporter {
    pub fn new(endpoint: &str, target_name: &str, az: &str) -> io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.connect(endpoint)?;

        let prefix = build_prefix(target_name, az);

        Ok(Self {
            // Sized from the actual prefix: a fixed 128 bytes would reallocate
            // on the first send once tags are long, undercutting the
            // no-allocation-on-the-hot-path property.
            buf: Vec::with_capacity(prefix.len().saturating_add(VALUE_HEADROOM)),
            socket,
            prefix,
        })
    }

    /// Sends one successful measurement. `ts_ns` is nanoseconds since UNIX epoch.
    pub fn send(&mut self, rtt_ms: f64, ts_ns: u64) -> io::Result<()> {
        format_line(&self.prefix, Some(rtt_ms), ts_ns, &mut self.buf);
        self.socket.send(&self.buf).map(|_| ())
    }

    /// Sends a failed probe: `success=0i` with no `rtt_ms` field, so averages
    /// over `rtt_ms` stay uncontaminated by a sentinel value.
    pub fn send_failure(&mut self, ts_ns: u64) -> io::Result<()> {
        format_line(&self.prefix, None, ts_ns, &mut self.buf);
        self.socket.send(&self.buf).map(|_| ())
    }
}

/// Builds the static tag prefix: `tcp_latency,target=<name>,az=<az> `.
fn build_prefix(target_name: &str, az: &str) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(80);
    prefix.extend_from_slice(b"tcp_latency,target=");
    prefix.extend_from_slice(target_name.as_bytes());
    prefix.extend_from_slice(b",az=");
    prefix.extend_from_slice(az.as_bytes());
    prefix.push(b' ');
    prefix
}

/// Renders one Influx Line Protocol line into `out`. `None` means the probe
/// failed, which omits `rtt_ms` entirely.
///
/// Free function taking the prefix rather than a method on the exporter: the
/// wire format is then testable without binding a UDP socket, so the format
/// tests run under a seccomp profile that forbids sockets and cannot fail for
/// reasons unrelated to formatting.
fn format_line(prefix: &[u8], rtt_ms: Option<f64>, ts_ns: u64, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(prefix);

    if let Some(rtt) = rtt_ms {
        out.extend_from_slice(b"rtt_ms=");
        let mut ryu_buf = ryu::Buffer::new();
        out.extend_from_slice(ryu_buf.format(rtt).as_bytes());
        out.extend_from_slice(b",success=1i");
    } else {
        out.extend_from_slice(b"success=0i");
    }

    out.push(b' ');
    let mut itoa_buf = itoa::Buffer::new();
    out.extend_from_slice(itoa_buf.format(ts_ns).as_bytes());
    // ILP requires a newline terminator; Telegraf socket_listener rejects lines without it.
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Formatting is tested with no socket at all: build the prefix and render
    /// into a buffer, exactly as `send`/`send_failure` do.
    fn prefix() -> Vec<u8> {
        build_prefix("mdb_primary", "eu-west")
    }

    #[test]
    fn success_line_carries_rtt_and_success_flag() {
        let mut out = Vec::new();
        format_line(&prefix(), Some(4.125), 1_686_561_230_000_000_000, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "tcp_latency,target=mdb_primary,az=eu-west rtt_ms=4.125,success=1i 1686561230000000000\n"
        );
    }

    #[test]
    fn failure_line_omits_rtt_so_averages_stay_clean() {
        let mut out = Vec::new();
        format_line(&prefix(), None, 1_686_561_231_000_000_000, &mut out);
        let line = String::from_utf8(out).unwrap();
        assert_eq!(
            line,
            "tcp_latency,target=mdb_primary,az=eu-west success=0i 1686561231000000000\n"
        );
        assert!(!line.contains("rtt_ms"));
    }

    #[test]
    fn every_line_is_newline_terminated() {
        // Telegraf's socket_listener silently drops lines without one.
        let mut out = Vec::new();
        format_line(&prefix(), Some(1.0), 1, &mut out);
        assert!(out.ends_with(b"\n"));
        format_line(&prefix(), None, 2, &mut out);
        assert!(out.ends_with(b"\n"));
    }

    #[test]
    fn the_buffer_is_reused_across_lines() {
        // format_line must clear, not append: a stale tail would produce two
        // measurements glued into one datagram.
        let mut out = Vec::new();
        format_line(&prefix(), Some(123.456), 1, &mut out);
        let long = out.len();
        format_line(&prefix(), None, 2, &mut out);
        assert!(out.len() < long, "buffer kept stale bytes: {out:?}");
        // Exactly one line: the only newline is the terminator.
        assert!(!out[..out.len() - 1].contains(&b'\n'));
    }

    /// The one test that does touch the network, kept separate from the format
    /// tests so a sandbox without sockets fails only here.
    #[test]
    fn send_reaches_a_udp_listener() {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sink.local_addr().unwrap().to_string();
        let mut e = TelegrafExporter::new(&addr, "mdb_primary", "eu-west").unwrap();

        e.send(4.125, 1_686_561_230_000_000_000).unwrap();

        let mut buf = [0u8; 256];
        let n = sink.recv(&mut buf).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf[..n]).unwrap(),
            "tcp_latency,target=mdb_primary,az=eu-west rtt_ms=4.125,success=1i 1686561230000000000\n"
        );
    }
}

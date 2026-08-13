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

        let mut prefix = Vec::with_capacity(80);
        prefix.extend_from_slice(b"tcp_latency,target=");
        prefix.extend_from_slice(target_name.as_bytes());
        prefix.extend_from_slice(b",az=");
        prefix.extend_from_slice(az.as_bytes());
        prefix.push(b' ');

        Ok(Self {
            // Sized from the actual prefix: a fixed 128 bytes would reallocate
            // on the first send once tags are long, undercutting the
            // no-allocation-on-the-hot-path property.
            buf: Vec::with_capacity(prefix.len() + VALUE_HEADROOM),
            socket,
            prefix,
        })
    }

    /// Sends one successful measurement. `ts_ns` is nanoseconds since UNIX epoch.
    pub fn send(&mut self, rtt_ms: f64, ts_ns: u64) -> io::Result<()> {
        self.buf.clear();
        self.buf.extend_from_slice(&self.prefix);

        self.buf.extend_from_slice(b"rtt_ms=");
        let mut ryu_buf = ryu::Buffer::new();
        self.buf
            .extend_from_slice(ryu_buf.format(rtt_ms).as_bytes());
        self.buf.extend_from_slice(b",success=1i");

        self.finish(ts_ns)
    }

    /// Sends a failed probe: `success=0i` with no `rtt_ms` field, so averages
    /// over `rtt_ms` stay uncontaminated by a sentinel value.
    pub fn send_failure(&mut self, ts_ns: u64) -> io::Result<()> {
        self.buf.clear();
        self.buf.extend_from_slice(&self.prefix);
        self.buf.extend_from_slice(b"success=0i");

        self.finish(ts_ns)
    }

    fn finish(&mut self, ts_ns: u64) -> io::Result<()> {
        self.buf.push(b' ');

        let mut itoa_buf = itoa::Buffer::new();
        self.buf
            .extend_from_slice(itoa_buf.format(ts_ns).as_bytes());
        // ILP requires a newline terminator; Telegraf socket_listener rejects lines without it.
        self.buf.push(b'\n');

        self.socket.send(&self.buf).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the exporter plus the receiving socket, which must be kept
    /// alive: the socket is *connected*, so sending to a closed port makes the
    /// kernel surface the resulting ICMP unreachable as ECONNREFUSED on the
    /// NEXT send. A live receiver keeps the test about formatting.
    fn exporter() -> (TelegrafExporter, UdpSocket) {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sink.local_addr().unwrap().to_string();
        (
            TelegrafExporter::new(&addr, "mdb_primary", "eu-west").unwrap(),
            sink,
        )
    }

    #[test]
    fn success_line_carries_rtt_and_success_flag() {
        let (mut e, _sink) = exporter();
        e.send(4.125, 1_686_561_230_000_000_000).unwrap();
        assert_eq!(
            String::from_utf8(e.buf.clone()).unwrap(),
            "tcp_latency,target=mdb_primary,az=eu-west rtt_ms=4.125,success=1i 1686561230000000000\n"
        );
    }

    #[test]
    fn failure_line_omits_rtt_so_averages_stay_clean() {
        let (mut e, _sink) = exporter();
        e.send_failure(1_686_561_231_000_000_000).unwrap();
        let line = String::from_utf8(e.buf.clone()).unwrap();
        assert_eq!(
            line,
            "tcp_latency,target=mdb_primary,az=eu-west success=0i 1686561231000000000\n"
        );
        assert!(!line.contains("rtt_ms"));
    }

    #[test]
    fn every_line_is_newline_terminated() {
        // Telegraf's socket_listener silently drops lines without one.
        let (mut e, _sink) = exporter();
        e.send(1.0, 1).unwrap();
        assert!(e.buf.ends_with(b"\n"));
        e.send_failure(2).unwrap();
        assert!(e.buf.ends_with(b"\n"));
    }
}

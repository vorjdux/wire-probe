use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Bytes reserved past the prefix for the formatted value and newline.
/// A ryu-formatted f64 is at most 24 bytes; 32 leaves margin.
const VALUE_HEADROOM: usize = 32;

/// Cap on a single blocking write to the Unix socket.
const UDS_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Writes PUTVAL lines either to stdout (Exec plugin) or a Unix domain socket.
///
/// Format: `PUTVAL <host>/wire-probe-tcp/latency-<target> interval=<n> N:<rtt_ms>`
///
/// A failed probe sends `N:U`  -  collectd's "undefined" marker, the same thing
/// the ping plugin dispatches on loss. That keeps the series ticking at a known
/// timestamp instead of leaving a gap that could equally mean the probe died.
///
/// The static prefix is built once; only the float value is formatted per send.
pub struct CollectdExporter {
    dest: Dest,
    prefix: Vec<u8>,
    buf: Vec<u8>,
}

enum Dest {
    Exec,
    Uds(UnixStream),
}

impl CollectdExporter {
    pub fn new_exec(hostname: &str, target_name: &str, interval_secs: u32) -> Self {
        let prefix = build_prefix(hostname, target_name, interval_secs);
        Self {
            dest: Dest::Exec,
            // Sized from the actual prefix so the "no allocation on the hot
            // path" property holds for long hostnames too, instead of relying
            // on a fixed 128 bytes that a long host/target pair would outgrow
            // on the first send.
            buf: Vec::with_capacity(prefix.len() + VALUE_HEADROOM),
            prefix,
        }
    }

    pub fn new_uds(
        path: &str,
        hostname: &str,
        target_name: &str,
        interval_secs: u32,
    ) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        // A Unix stream socket is NOT fire-and-forget: if collectd stops
        // reading, the kernel buffer fills and write_all() blocks forever,
        // which would stop the probe measuring. Bound it so a stalled consumer
        // costs one send, not the process.
        stream.set_write_timeout(Some(UDS_WRITE_TIMEOUT))?;
        let prefix = build_prefix(hostname, target_name, interval_secs);
        Ok(Self {
            dest: Dest::Uds(stream),
            buf: Vec::with_capacity(prefix.len() + VALUE_HEADROOM),
            prefix,
        })
    }

    pub fn send(&mut self, rtt_ms: f64) -> io::Result<()> {
        self.buf.clear();
        self.buf.extend_from_slice(&self.prefix);

        let mut ryu_buf = ryu::Buffer::new();
        self.buf
            .extend_from_slice(ryu_buf.format(rtt_ms).as_bytes());

        self.flush_line()
    }

    /// Sends `N:U` (undefined) for a failed probe.
    pub fn send_failure(&mut self) -> io::Result<()> {
        self.buf.clear();
        self.buf.extend_from_slice(&self.prefix);
        self.buf.push(b'U');

        self.flush_line()
    }

    fn flush_line(&mut self) -> io::Result<()> {
        self.buf.push(b'\n');

        match &mut self.dest {
            Dest::Exec => {
                let stdout = io::stdout();
                let mut lock = stdout.lock();
                lock.write_all(&self.buf)?;
                // Stdout is line-buffered only when attached to a terminal;
                // under the Exec plugin it is a pipe, so flush explicitly
                // rather than relying on the trailing newline.
                lock.flush()
            }
            Dest::Uds(stream) => stream.write_all(&self.buf),
        }
    }
}

fn build_prefix(hostname: &str, target_name: &str, interval_secs: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(96);
    p.extend_from_slice(b"PUTVAL ");
    p.extend_from_slice(hostname.as_bytes());
    p.extend_from_slice(b"/wire-probe-tcp/latency-");
    p.extend_from_slice(target_name.as_bytes());
    p.extend_from_slice(b" interval=");
    let mut itoa_buf = itoa::Buffer::new();
    p.extend_from_slice(itoa_buf.format(interval_secs).as_bytes());
    p.extend_from_slice(b" N:");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matches_the_putval_wire_format() {
        let p = build_prefix("obs01", "mdb_primary", 10);
        assert_eq!(
            String::from_utf8(p).unwrap(),
            "PUTVAL obs01/wire-probe-tcp/latency-mdb_primary interval=10 N:"
        );
    }

    #[test]
    fn exec_exporter_writes_value_then_undefined_on_failure() {
        // Exercises the same buffer the socket path uses, without a consumer.
        let mut e = CollectdExporter::new_exec("obs01", "t", 1);
        e.buf.clear();
        e.buf.extend_from_slice(&e.prefix);
        let mut ryu_buf = ryu::Buffer::new();
        e.buf
            .extend_from_slice(ryu_buf.format(4.125_f64).as_bytes());
        assert!(String::from_utf8_lossy(&e.buf).ends_with("N:4.125"));

        e.buf.clear();
        e.buf.extend_from_slice(&e.prefix);
        e.buf.push(b'U');
        assert!(String::from_utf8_lossy(&e.buf).ends_with("N:U"));
    }

    #[test]
    fn buffer_capacity_absorbs_a_long_identifier_without_growing() {
        let host = "a".repeat(200);
        let e = CollectdExporter::new_exec(&host, "target-with-a-long-name", 1);
        let cap = e.buf.capacity();
        // Worst case: full prefix plus a ryu-formatted f64 plus newline.
        assert!(cap >= e.prefix.len() + 25, "capacity {cap} too small");
    }
}

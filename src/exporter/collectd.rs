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
    /// The path is kept so the stream can be reopened: a write that fails
    /// part-way leaves half a PUTVAL line in the socket, and a collectd
    /// restart leaves the stream dead for the life of the process.
    Uds {
        path: String,
        stream: UnixStream,
    },
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
            buf: Vec::with_capacity(prefix.len().saturating_add(VALUE_HEADROOM)),
            prefix,
        }
    }

    pub fn new_uds(
        path: &str,
        hostname: &str,
        target_name: &str,
        interval_secs: u32,
    ) -> io::Result<Self> {
        let stream = connect_uds(path)?;
        let prefix = build_prefix(hostname, target_name, interval_secs);
        Ok(Self {
            dest: Dest::Uds {
                path: path.to_string(),
                stream,
            },
            buf: Vec::with_capacity(prefix.len().saturating_add(VALUE_HEADROOM)),
            prefix,
        })
    }

    pub fn send(&mut self, rtt_ms: f64) -> io::Result<()> {
        self.format_line(Some(rtt_ms));
        self.flush_line()
    }

    /// Sends `N:U` (undefined) for a failed probe.
    pub fn send_failure(&mut self) -> io::Result<()> {
        self.format_line(None);
        self.flush_line()
    }

    /// Renders one PUTVAL line into `buf`: the value, or `U` when the probe
    /// failed. Separated from the I/O so tests exercise the same formatting
    /// the send path uses, instead of a copy of it  -  the Exec destination
    /// writes to stdout, where a test has nothing to inspect.
    fn format_line(&mut self, rtt_ms: Option<f64>) {
        self.buf.clear();
        self.buf.extend_from_slice(&self.prefix);

        match rtt_ms {
            Some(rtt) => {
                let mut ryu_buf = ryu::Buffer::new();
                self.buf.extend_from_slice(ryu_buf.format(rtt).as_bytes());
            }
            None => self.buf.push(b'U'),
        }
        self.buf.push(b'\n');
    }

    fn flush_line(&mut self) -> io::Result<()> {
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
            Dest::Uds { path, stream } => {
                // write_all() on a stream socket can write part of the line and
                // then hit the write timeout, and it does not report how much
                // got through. Half a PUTVAL line stays in the socket, and the
                // next send appends a fresh prefix to that half, handing
                // collectd a corrupt line. Reconnecting after any write error
                // discards the torn frame, and also recovers the case where
                // collectd restarted and left this stream dead forever.
                match stream.write_all(&self.buf) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        *stream = connect_uds(path)?;
                        Err(e)
                    }
                }
            }
        }
    }
}

/// Connects to the collectd `UnixSock` plugin with a bounded write timeout: a
/// stream socket blocks once the kernel buffer fills, which would stop the
/// probe measuring entirely if collectd stopped reading.
fn connect_uds(path: &str) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(UDS_WRITE_TIMEOUT))?;
    Ok(stream)
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
    fn format_line_renders_value_and_undefined_marker() {
        // Calls the same formatting `send`/`send_failure` call, rather than
        // reproducing it: a test that rebuilds the body only ever checks its
        // own copy.
        let mut e = CollectdExporter::new_exec("obs01", "mdb_primary", 10);

        e.format_line(Some(4.125));
        assert_eq!(
            String::from_utf8(e.buf.clone()).unwrap(),
            "PUTVAL obs01/wire-probe-tcp/latency-mdb_primary interval=10 N:4.125\n"
        );

        e.format_line(None);
        assert_eq!(
            String::from_utf8(e.buf.clone()).unwrap(),
            "PUTVAL obs01/wire-probe-tcp/latency-mdb_primary interval=10 N:U\n"
        );
    }

    #[test]
    fn uds_send_writes_the_line_a_reader_can_parse() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("wire-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("collectd.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let mut e =
            CollectdExporter::new_uds(sock.to_str().unwrap(), "obs01", "mdb_primary", 10).unwrap();
        let (server, _) = listener.accept().unwrap();

        e.send(4.125).unwrap();
        e.send_failure().unwrap();

        let mut reader = BufReader::new(server);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            line,
            "PUTVAL obs01/wire-probe-tcp/latency-mdb_primary interval=10 N:4.125\n"
        );
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            line,
            "PUTVAL obs01/wire-probe-tcp/latency-mdb_primary interval=10 N:U\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uds_reconnects_after_the_consumer_goes_away() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("wire-probe-reconnect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("collectd.sock");
        let _ = std::fs::remove_file(&sock);

        let listener = UnixListener::bind(&sock).unwrap();
        let mut e = CollectdExporter::new_uds(sock.to_str().unwrap(), "obs01", "t", 1).unwrap();
        let (server, _) = listener.accept().unwrap();

        // collectd goes away and comes back on the same path. Dropping a
        // UnixListener does not unlink the socket file, so remove it first.
        drop(server);
        drop(listener);
        std::fs::remove_file(&sock).unwrap();
        let listener = UnixListener::bind(&sock).unwrap();

        // The first send hits the dead peer: it reports the error and
        // reconnects on the way out. The next one lands on the new socket,
        // which is the behaviour that keeps a collectd restart from leaving
        // this exporter mute for the life of the process.
        assert!(
            e.send(1.0).is_err(),
            "writing to a closed peer should report an error"
        );
        e.send(2.0)
            .expect("exporter should have reconnected after the failed write");

        let (server, _) = listener.accept().unwrap();
        e.send(4.125).unwrap();

        // Whole lines arrive, in order: no torn frame from the failed write.
        let mut reader = BufReader::new(server);
        let mut first = String::new();
        let mut second = String::new();
        reader.read_line(&mut first).unwrap();
        reader.read_line(&mut second).unwrap();
        assert!(first.ends_with("N:2.0\n"), "got: {first}");
        assert!(second.ends_with("N:4.125\n"), "got: {second}");

        std::fs::remove_dir_all(&dir).ok();
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

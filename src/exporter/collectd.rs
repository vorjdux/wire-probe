use std::io::{self, Write};
use std::os::unix::net::UnixStream;

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
        Self {
            dest: Dest::Exec,
            prefix: build_prefix(hostname, target_name, interval_secs),
            buf: Vec::with_capacity(128),
        }
    }

    pub fn new_uds(
        path: &str,
        hostname: &str,
        target_name: &str,
        interval_secs: u32,
    ) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self {
            dest: Dest::Uds(stream),
            prefix: build_prefix(hostname, target_name, interval_secs),
            buf: Vec::with_capacity(128),
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

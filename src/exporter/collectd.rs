use std::io::{self, Write};
use std::os::unix::net::UnixStream;

/// Writes PUTVAL lines either to stdout (Exec plugin) or a Unix domain socket.
///
/// Format: `PUTVAL <host>/wire-probe-tcp/latency-<target> interval=<n> N:<rtt_ms>`
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
        self.buf.push(b'\n');

        match &mut self.dest {
            Dest::Exec => io::stdout().write_all(&self.buf),
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

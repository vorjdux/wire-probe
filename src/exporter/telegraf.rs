use std::io;
use std::net::UdpSocket;

/// Formats and ships Influx Line Protocol measurements to a Telegraf
/// `[[inputs.socket_listener]]` UDP endpoint.
///
/// The static prefix (`tcp_latency,target=<name>,az=<az> rtt_ms=`) is built
/// once at construction; only the float value and timestamp are formatted on
/// each send, with zero heap allocation on the hot path.
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
        prefix.extend_from_slice(b" rtt_ms=");

        Ok(Self {
            socket,
            prefix,
            buf: Vec::with_capacity(128),
        })
    }

    /// Sends one measurement. `ts_ns` is nanoseconds since UNIX epoch.
    pub fn send(&mut self, rtt_ms: f64, ts_ns: u64) -> io::Result<()> {
        self.buf.clear();
        self.buf.extend_from_slice(&self.prefix);

        let mut ryu_buf = ryu::Buffer::new();
        self.buf.extend_from_slice(ryu_buf.format(rtt_ms).as_bytes());
        self.buf.push(b' ');

        let mut itoa_buf = itoa::Buffer::new();
        self.buf.extend_from_slice(itoa_buf.format(ts_ns).as_bytes());
        // ILP requires a newline terminator; Telegraf socket_listener rejects lines without it.
        self.buf.push(b'\n');

        self.socket.send(&self.buf).map(|_| ())
    }
}

# wire-probe

> Zero-footprint L4 telemetry agent  -  TCP handshake RTT, sub-1 MB RAM, no runtime deps.

[![CI](https://github.com/vorjdux/wire-probe/actions/workflows/ci.yml/badge.svg)](https://github.com/vorjdux/wire-probe/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/vorjdux/wire-probe)](https://github.com/vorjdux/wire-probe/releases/latest)

## Why

ICMP ping is blocked by most firewalls and tells you nothing about the TCP stack.
`wire-probe` measures the time for a full TCP three-way handshake  -  the same
latency a real client sees  -  with no kernel module, no eBPF, no agent framework.

- **Server mode**  -  `io_uring` accept/drop loop; accepts SYNs and immediately
  closes them, consuming ~500 KB RSS at any connection rate
- **Probe mode**  -  blocking `TcpStream::connect_timeout` wrapped with
  `std::time::Instant`; the `--timeout` flag is the only timing bound
- **Fire-and-forget export**  -  UDP (Telegraf Influx Line Protocol) or Unix
  domain socket / stdout (Collectd PUTVAL); no buffering, no retries
- **Single static binary**  -  musl-linked, 370 KB stripped, zero glibc version
  dependency, works on Ubuntu, Fedora, Debian, RHEL, Alpine and any other Linux

## Install

```bash
curl -sSf https://raw.githubusercontent.com/vorjdux/wire-probe/main/install.sh | sh
```

Or install a specific version:

```bash
curl -sSf https://raw.githubusercontent.com/vorjdux/wire-probe/main/install.sh | VERSION=0.1.2 sh
```

Pre-built tarballs for every platform are on the [releases page](https://github.com/vorjdux/wire-probe/releases/latest).

| Platform | Tarball |
|---|---|
| Any Linux x86_64 (static) | `wire-probe-<ver>-linux-x86_64.tar.gz` |
| Any Linux aarch64 (static) | `wire-probe-<ver>-linux-aarch64.tar.gz` |
| Ubuntu 22.04 | `wire-probe-<ver>-ubuntu22.04-x86_64.tar.gz` |
| Ubuntu 24.04 | `wire-probe-<ver>-ubuntu24.04-x86_64.tar.gz` |
| Fedora 40 / 41 | `wire-probe-<ver>-fedora40-x86_64.tar.gz` |
| Debian 12 | `wire-probe-<ver>-debian12-x86_64.tar.gz` |
| Rocky Linux 9 / AlmaLinux 9 | `wire-probe-<ver>-rockylinux9-x86_64.tar.gz` |

> The `linux-x86_64` and `linux-aarch64` tarballs are statically linked against
> musl libc and run on any Linux kernel ≥ 5.1 regardless of distro or glibc version.

## Quick start

**On the target host (DB node):**

```bash
wire-probe --mode server --port 9999
```

**On the observer host (PLT node):**

```bash
# Export to Telegraf via UDP (Influx Line Protocol)
wire-probe --mode probe \
  --target db-host:9999 \
  --target-name mdb_primary \
  --az eu-west \
  --interval 1000ms \
  --export telegraf-udp://127.0.0.1:8094

# Export to Collectd via stdout (Exec plugin)
wire-probe --mode probe \
  --target db-host:9999 \
  --target-name mdb_primary \
  --interval 10s \
  --export collectd-exec
```

## Modes

### Server mode

Runs as a resident daemon on the target host. Uses an `io_uring` accept/drop
loop  -  accepts each TCP SYN and immediately closes the socket without reading
any data. No thread is spawned per connection.

```
wire-probe --mode server [--port <port>] [--bind <addr>]
```

| Flag | Default | Description |
|---|---|---|
| `--port` | `9999` | TCP port to listen on |
| `--bind` | `0.0.0.0` | Bind address (use a private IP to restrict exposure) |

#### Running under systemd

A ready-made unit ships in
[`packaging/systemd/wire-probe-server.service`](packaging/systemd/wire-probe-server.service):

```bash
curl -sSfL https://raw.githubusercontent.com/vorjdux/wire-probe/main/packaging/systemd/wire-probe-server.service \
  -o /etc/systemd/system/wire-probe-server.service

# adjust --port / --bind if needed
systemctl edit --full wire-probe-server.service

systemctl daemon-reload
systemctl enable --now wire-probe-server
systemctl status wire-probe-server
```

The unit assumes the binary is at `/usr/local/bin/wire-probe` (the default
`install.sh` location when run as root) and restarts on failure with backoff.

It runs under `DynamicUser=yes` with a locked-down sandbox: no capabilities, a
read-only filesystem, `AF_INET`/`AF_INET6` only, and `SystemCallFilter=@system-service`
(which nests `@aio`, where the `io_uring` syscalls live). `systemd-analyze
security` rates it 1.3 OK, against 9.4 UNSAFE for the same unit unhardened.
Binding a port below 1024 needs `CAP_NET_BIND_SERVICE` added back  -  see the
commented lines in the unit.

### Probe mode

Runs on the observer host. Measures the TCP handshake RTT to the target and
exports the result on every interval. The target address is resolved once at
startup  -  use an IP address if DNS reliability on your network is a concern.

```
wire-probe --mode probe --target <host:port> [options]
```

| Flag | Default | Description |
|---|---|---|
| `--target` | *(required)* | `host:port` of the wire-probe server |
| `--target-name` | derived from `--target` | Label used in metric names |
| `--az` | `default` | Availability-zone tag (Telegraf only) |
| `--interval` | `1000ms` | Time between probes (`ms` or `s` suffix); min 100ms, max 24h |
| `--timeout` | `5000ms` | Connect timeout per probe; max 60s |
| `--export` | `collectd-exec` | Export destination (see below) |

## Export targets

### Telegraf  -  Influx Line Protocol over UDP

```
--export telegraf-udp://<host>:<port>
```

Sends one UDP datagram per probe in [Influx Line Protocol](https://docs.influxdata.com/influxdb/cloud/reference/syntax/line-protocol/) format:

```
tcp_latency,target=mdb_primary,az=eu-west rtt_ms=4.12,success=1i 1686561230000000000
```

A failed probe still emits a point, carrying `success=0i` and no `rtt_ms`:

```
tcp_latency,target=mdb_primary,az=eu-west success=0i 1686561231000000000
```

This is deliberate. Over fire-and-forget UDP a missing point is
indistinguishable from a lost datagram, so alerting on absence cannot separate
"target down" from "probe down". Alert on `success` instead, and note that
`rtt_ms` is absent rather than zero on failure, so averages stay clean.

Telegraf configuration:

```toml
[[inputs.socket_listener]]
  service_address = "udp://127.0.0.1:8094"
  data_format     = "influx"
```

### Collectd  -  PUTVAL

#### Exec plugin (stdout)

```
--export collectd-exec
```

Writes `PUTVAL` lines to stdout. Use with collectd's
[Exec plugin](https://collectd.org/wiki/index.php/Plugin:Exec):

```
PUTVAL hostname/wire-probe-tcp/latency-mdb_primary interval=10 N:4.12
```

A failed probe sends `N:U`, collectd's "undefined" marker, rather than nothing:

```
PUTVAL hostname/wire-probe-tcp/latency-mdb_primary interval=10 N:U
```

```xml
<Plugin exec>
  Exec "nobody" "/usr/local/bin/wire-probe"
       "--mode"   "probe"
       "--target" "db-host:9999"
       "--target-name" "mdb_primary"
       "--interval" "10s"
       "--export" "collectd-exec"
</Plugin>
```

#### Unix domain socket

```
--export collectd-uds:///var/run/collectd-unixsock
```

Streams `PUTVAL` lines directly to collectd's
[UnixSock plugin](https://collectd.org/wiki/index.php/Plugin:UnixSock).

## Collectd Python plugin

A drop-in replacement for collectd's `ping` plugin  -  same value types
(`ping`, `ping_droprate`, `ping_stddev`), no recompilation of collectd
required.

Being a Python module, it is loaded via the `python` plugin  -  **not** with a
bare `<Plugin wire_probe>` block (that will not load the module):

```xml
LoadPlugin python

<Plugin python>
  ModulePath "/usr/lib/collectd/wire_probe"
  Import "wire_probe"

  <Module wire_probe>
    Host "db-node-01"
    Host "db-node-02"
    Host "app-node-01"

    Port      9999
    Timeout   5.0
    PingCount 1
  </Module>
</Plugin>
```

`LoadPlugin python` is only needed if the python plugin is not already loaded
elsewhere in `collectd.conf`  -  drop that line if it is, to avoid a duplicate
`LoadPlugin` warning at startup.

> **Where the data lands in InfluxDB:** collectd's InfluxDB naming derives the
> measurement from the *plugin* name, not the value type  -  so probes appear in
> **`wire_probe_value`** (tag `type` in `ping`/`ping_droprate`/`ping_stddev`,
> `type_instance` = target host), **not** in `ping_value`. To fold the data
> into the existing `ping_value` measurement instead, set `v.plugin = "ping"`
> in `wire_probe.py`  -  at the cost of mixing L4 RTT with real ICMP ping.

Install:

```bash
curl -sSf https://raw.githubusercontent.com/vorjdux/wire-probe/main/install-plugin.sh | sudo sh
```

Or manually:

```bash
mkdir -p /usr/lib/collectd/wire_probe
cp plugin/collectd/wire_probe.py /usr/lib/collectd/wire_probe/
cp plugin/collectd/wire_probe.conf /etc/collectd/conf.d/
systemctl restart collectd   # collectd has no reload
```

See [`plugin/collectd/wire_probe.conf`](plugin/collectd/wire_probe.conf) for
the full configuration reference.

## Build from source

Requires Rust 1.85+ (the crate is on `edition = "2024"`) and Linux kernel
>= 5.1 (for `io_uring`).

```bash
git clone https://github.com/vorjdux/wire-probe
cd wire-probe
cargo build --release
```

Static musl binary (runs anywhere):

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Design

| Concern | Decision |
|---|---|
| Async runtime | None  -  `io_uring` for the server accept loop, blocking threads for probes |
| Memory baseline | ~500 KB RSS (server), ~300 KB RSS (probe) |
| Export allocations | Zero  -  `ryu`/`itoa` format into pre-allocated stack buffers |
| Export reliability | Fire-and-forget; no retry, no queue  -  if Telegraf/Collectd is down, the probe skips |
| Binary size | 370 KB stripped (fat LTO, `panic = "abort"`, `strip = true`) |

## Behind the Design

`wire-probe` was built to measure pure L3/L4 Data Plane latency, completely isolated from SDN Control Plane throttling (which heavily skews ICMP ping on cloud providers like Azure) and L7 application bottlenecks. To achieve a zero-footprint observer effect, every architectural decision prioritized bypassing userland overhead.

### 1. Bypassing Async Runtimes for Direct Kernel Interfaces

Including a standard async runtime (like `tokio`) imposes an unacceptable baseline memory footprint (2–5 MB RSS) and scheduler overhead for a binary whose sole purpose is handling socket file descriptors.

- **Server mode (`io_uring`):** The TCP accept loop is submitted to the Linux kernel's asynchronous submission/completion queues via `io_uring`. The daemon maintains an RSS under ~500 KB regardless of load because there are no per-connection allocations  -  accepted fds are closed immediately with a plain `libc::close`. Note: the current implementation uses a single outstanding accept (serial re-arm per connection); throughput is bounded by one `submit_and_wait` syscall per connection, which is sufficient for telemetry use but not for high-PPS scenarios.
- **Probe mode (native blocking):** Uses `TcpStream::connect_timeout` on a blocking thread. The `--timeout` flag is the only timing bound  -  it maps directly to the OS-level connect timeout. The RTT is captured by `Instant::now()` around the connect call; no other mechanism is involved. DNS is resolved once at startup to avoid per-tick `getaddrinfo` blocking inside the measurement loop.

### 2. Zero-Allocation Export Path and Binary Density

To guarantee the export hot path runs within the CPU's L1/L2 caches and avoids memory fragmentation during long-running execution, the binary is extremely dense.

- Compiled statically via `musl-libc` with fat LTO and `panic = "abort"`, producing a ~370 KB stripped binary (~500 KB RSS at runtime for the server, ~300 KB for the probe).
- Heap allocations are eliminated on the **export** hot path: the metric prefix is built once at construction, the send buffer is reused via `clear()`, and `ryu`/`itoa` write directly into stack-allocated buffers with no `format!` or `String` intermediary.

### 3. Fire-and-Forget Export and Backpressure Offloading

An observability probe must not block, and must not be brought down, because the
downstream telemetry pipeline degraded. Note that the release profile sets
`panic = "abort"`, so a panic terminates the process rather than unwinding  -  the
guarantee is that no export path *blocks* or propagates backpressure, not that the
process is panic-proof. Restart supervision is the systemd unit's job.

- By forcing metric injection via UDP datagrams (Telegraf/Influx) or Unix domain sockets (Collectd), `wire-probe` structurally outsources backpressure handling to the Linux kernel.
- If the destination TSDB stalls or the Telegraf process hangs, the kernel applies a silent tail-drop at the receive buffer. This isolates the probe, shielding it from file descriptor exhaustion or OOM kills.

### 4. Pragmatic Collectd Integration

Collectd's `Exec` plugin runs a child process once and reads its stdout in a long-lived loop  -  it does not re-fork per interval. The real cost it imposes is an out-of-process boundary: every read cycle crosses a pipe, a process boundary, and a shell.

- `wire_probe.py` is a full Python reimplementation of the probe logic (not a wrapper around the Rust binary). It registers directly with collectd's C runtime via the Python plugin API (`register_read` callback), running in-process with no child process at all. This eliminates the pipe/process boundary entirely. The trade-off: the Rust binary's musl, zero-alloc, and `io_uring` properties do not apply on this path  -  you get CPython doing blocking `socket.create_connection` calls. For collectd environments the in-process scheduling and drop-in `ping`/`ping_droprate`/`ping_stddev` metric names make it the right integration point.

## License

MIT  -  see [LICENSE](LICENSE).

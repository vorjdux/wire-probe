"""
wire_probe.py  -  collectd plugin: TCP-handshake RTT, drop-rate, and stddev.

Mirrors the collectd ping plugin but uses TCP connect() (L4) instead of ICMP.
Reuses the same value types so existing ping dashboards and thresholds work
without changes: ping, ping_droprate, ping_stddev.

Configuration (inside <Module wire_probe>):
  Host      "hostname"          # bare hostname uses the global Port
  Host      "hostname:port"     # per-host port override
  Port      9999                # default TCP port (wire-probe server listener)
  Timeout   5.0                 # connect timeout in seconds (float)
  PingCount 1                   # RTT samples averaged per read cycle
"""

import collectd
import math
import socket
import time

PLUGIN_NAME = "wire_probe"

# Fraction of the configured collectd Interval that one read_cb invocation may
# consume across all hosts. Derived from the live Interval rather than fixed:
# with "Interval 1" a hardcoded 8s budget would block collectd's read thread
# for eight times its own cycle.
_READ_BUDGET_RATIO = 0.8
# Used only if collectd.get_interval() is unavailable (older collectd).
_FALLBACK_INTERVAL_S = 10.0


def _read_budget():
    try:
        interval = float(collectd.get_interval())
    except (AttributeError, TypeError, ValueError):
        interval = _FALLBACK_INTERVAL_S
    if interval <= 0:
        interval = _FALLBACK_INTERVAL_S
    return interval * _READ_BUDGET_RATIO

_hosts = []       # list of (display_name, host, port)
_timeout = 5.0
_ping_count = 1
_default_port = 9999


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _valid_port(port):
    return 1 <= port <= 65535


def _resolve(host, port):
    """Return (family, sockaddr) for the first address, or None on failure.

    Resolution is deliberately kept OUT of the timed section: socket.
    create_connection() would fold getaddrinfo() into the measurement, so a
    cold DNS lookup shows up as multi-second "RTT" on the first read cycle.
    Only the first address is used, mirroring the Rust probe (to_socket_addrs
    followed by .next()).
    """
    try:
        infos = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except OSError:
        return None
    if not infos:
        return None
    family, _, _, _, sockaddr = infos[0]
    return family, sockaddr


def _tcp_rtt(addr_info, timeout):
    """Return TCP handshake RTT in milliseconds, or None on failure."""
    family, sockaddr = addr_info
    sock = socket.socket(family, socket.SOCK_STREAM)
    try:
        sock.settimeout(timeout)
        t0 = time.monotonic()
        sock.connect(sockaddr)
        return (time.monotonic() - t0) * 1_000.0
    except OSError:
        return None
    finally:
        sock.close()


def _emit(type_instance, value_type, value):
    v = collectd.Values()
    v.plugin = PLUGIN_NAME
    v.type = value_type
    v.type_instance = type_instance
    v.values = [value]
    v.dispatch()


# ---------------------------------------------------------------------------
# collectd callbacks
# ---------------------------------------------------------------------------

def config_cb(conf):
    global _timeout, _ping_count, _default_port

    for node in conf.children:
        key = node.key.lower()

        if key == "host":
            raw = node.values[0]
            # Support "hostname:port" inline override
            if ":" in raw and not raw.startswith("["):
                # plain IPv4 host:port  (IPv6 literals would be [::1]:port)
                parts = raw.rsplit(":", 1)
                try:
                    inline_port = int(parts[1])
                    if not _valid_port(inline_port):
                        collectd.warning(
                            f"{PLUGIN_NAME}: inline port {inline_port} in '{raw}' "
                            f"out of range [1, 65535], using default port"
                        )
                        hostname, port = parts[0], None
                    else:
                        hostname, port = parts[0], inline_port
                except ValueError:
                    hostname, port = raw, None  # resolve port later
            else:
                hostname, port = raw, None
            # De-duplicate rather than reset the list: collectd calls this once
            # per <Module wire_probe> block, so clearing would discard all but
            # the last block, while blind appending probes a repeated target
            # once per duplicate entry.
            entry = (raw, hostname, port)
            if entry in _hosts:
                collectd.warning(f"{PLUGIN_NAME}: duplicate Host '{raw}', ignoring")
            else:
                _hosts.append(entry)

        elif key == "port":
            port_val = int(node.values[0])
            if not _valid_port(port_val):
                collectd.warning(f"{PLUGIN_NAME}: Port {port_val} out of range [1, 65535], using default {_default_port}")
            else:
                _default_port = port_val

        elif key == "timeout":
            t = float(node.values[0])
            if t <= 0 or t > 60.0:
                collectd.warning(f"{PLUGIN_NAME}: Timeout {t} out of range (0, 60], using default {_timeout}")
            else:
                _timeout = t

        elif key == "pingcount":
            pc = int(node.values[0])
            if not 1 <= pc <= 100:
                collectd.warning(f"{PLUGIN_NAME}: PingCount {pc} out of range [1, 100], using default {_ping_count}")
            else:
                _ping_count = pc

        else:
            collectd.warning(f"{PLUGIN_NAME}: unknown config key '{node.key}'")


def init_cb():
    collectd.info(
        f"{PLUGIN_NAME}: probing {len(_hosts)} host(s), "
        f"port={_default_port}, timeout={_timeout}s, "
        f"ping_count={_ping_count}"
    )


def read_cb():
    # Snapshot config so a concurrent reload cannot mutate it mid-read.
    timeout = _timeout
    ping_count = _ping_count
    default_port = _default_port

    # Hard deadline: stop probing when the global budget is exhausted so this
    # callback never blocks collectd's read thread for most of its Interval.
    deadline = time.monotonic() + _read_budget()

    for display, hostname, host_port in _hosts:
        port = host_port if host_port is not None else default_port

        # Resolved once per read cycle, outside the timed section, and reused
        # for every sample. A resolution failure counts as a full drop.
        addr_info = _resolve(hostname, port)
        if addr_info is None:
            collectd.warning(f"{PLUGIN_NAME}: cannot resolve '{hostname}', counting as drop")

        samples = []
        for _ in range(ping_count):
            if addr_info is None:
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                collectd.warning(f"{PLUGIN_NAME}: read budget exhausted, skipping remaining probes")
                break
            rtt = _tcp_rtt(addr_info, min(timeout, remaining))
            if rtt is not None:
                samples.append(rtt)

        drops = ping_count - len(samples)
        droprate = drops / ping_count

        if samples:
            avg = sum(samples) / len(samples)
            if len(samples) > 1:
                variance = sum((r - avg) ** 2 for r in samples) / len(samples)
                stddev = math.sqrt(variance)
            else:
                stddev = float("nan")
        else:
            avg = float("nan")
            stddev = float("nan")

        # type_instance mirrors the ping plugin: bare hostname (no port)
        _emit(hostname, "ping", avg)
        _emit(hostname, "ping_droprate", droprate)
        _emit(hostname, "ping_stddev", stddev)


collectd.register_config(config_cb)
collectd.register_init(init_cb)
collectd.register_read(read_cb)

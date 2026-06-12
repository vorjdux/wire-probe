"""
wire_probe.py — collectd plugin: TCP-handshake RTT, drop-rate, and stddev.

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

# Total wall-clock budget for one read_cb invocation across all hosts.
# Keeps the callback under collectd's default 10s Interval regardless of
# how many hosts or samples are configured.
_READ_BUDGET_S = 8.0

_hosts = []       # list of (display_name, host, port)
_timeout = 5.0
_ping_count = 1
_default_port = 9999


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _valid_port(port):
    return 1 <= port <= 65535


def _tcp_rtt(host, port, timeout):
    """Return RTT in milliseconds, or None on connection failure."""
    try:
        t0 = time.monotonic()
        with socket.create_connection((host, port), timeout=timeout):
            pass
        return (time.monotonic() - t0) * 1_000.0
    except Exception:
        return None


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
            _hosts.append((raw, hostname, port))

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

    # Hard deadline: stop probing when the global budget is exhausted so
    # this callback never blocks collectd's read thread past _READ_BUDGET_S.
    deadline = time.monotonic() + _READ_BUDGET_S

    for display, hostname, host_port in _hosts:
        port = host_port if host_port is not None else default_port

        samples = []
        for _ in range(ping_count):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                collectd.warning(f"{PLUGIN_NAME}: read budget exhausted, skipping remaining probes")
                break
            rtt = _tcp_rtt(hostname, port, min(timeout, remaining))
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

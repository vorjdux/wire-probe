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
# How PingCount samples collapse into the reported `ping` value.
#
# "avg" matches the collectd ping plugin and is the default so existing series
# keep their meaning. "min" exists because this plugin runs inside CPython:
# the measurement is monotonic() around connect(), so any delay in re-acquiring
# the GIL after connect() returns lands in the value as if it were network
# latency. Measured on loopback, where the true RTT is 0.007 ms:
#
#   idle                        p50 0.007 ms   p99 0.032 ms
#   with GIL contention         p50 41 ms      p99 461 ms
#
# With PingCount 3 under that contention, avg reported p50 27 ms while min
# reported p50 5 ms. The minimum of N samples is the one least contaminated by
# scheduling, so "min" is the better estimator of the actual handshake on a
# busy collectd host. It cannot help at PingCount 1, where there is nothing to
# choose between.
_aggregate = "avg"


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


def _split_host_port(raw):
    """Split a Host value into (hostname, port-or-None).

    Accepted forms:
      hostname            hostname:port
      192.0.2.1           192.0.2.1:9999
      ::1                 [::1]:9999          [::1]

    Bare IPv6 literals must NOT be split on the last colon: "::1" would
    otherwise become host ":" port 1, which resolves to something unrelated
    without ever warning. Brackets are stripped because getaddrinfo() wants
    the address, not the bracketed URL form.
    """
    if raw.startswith("["):
        addr, sep, rest = raw.partition("]")
        host = addr[1:]
        if not sep:
            collectd.warning(f"{PLUGIN_NAME}: unbalanced '[' in Host '{raw}'")
            return raw, None
        if rest.startswith(":"):
            return host, _parse_port(rest[1:], raw)
        return host, None

    # Two or more colons means a bare IPv6 literal, which carries no port.
    if raw.count(":") >= 2:
        return raw, None

    if ":" in raw:
        host, _, port_str = raw.rpartition(":")
        return host, _parse_port(port_str, raw)

    return raw, None


def _parse_port(port_str, raw):
    """Parse an inline port, or None (falling back to the global Port)."""
    try:
        port = int(port_str)
    except ValueError:
        collectd.warning(f"{PLUGIN_NAME}: non-numeric port in Host '{raw}', using default port")
        return None
    if not _valid_port(port):
        collectd.warning(
            f"{PLUGIN_NAME}: inline port {port} in '{raw}' "
            f"out of range [1, 65535], using default port"
        )
        return None
    return port


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
    global _timeout, _ping_count, _default_port, _aggregate

    for node in conf.children:
        key = node.key.lower()

        if key == "host":
            raw = node.values[0]
            hostname, port = _split_host_port(raw)
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

        elif key == "aggregate":
            mode = str(node.values[0]).lower()
            if mode not in ("avg", "min"):
                collectd.warning(
                    f"{PLUGIN_NAME}: Aggregate '{node.values[0]}' unknown "
                    f"(expected avg or min), using default {_aggregate}"
                )
            else:
                _aggregate = mode

        else:
            collectd.warning(f"{PLUGIN_NAME}: unknown config key '{node.key}'")


def init_cb():
    collectd.info(
        f"{PLUGIN_NAME}: probing {len(_hosts)} host(s), "
        f"port={_default_port}, timeout={_timeout}s, "
        f"ping_count={_ping_count}, aggregate={_aggregate}"
    )


def read_cb():
    # Snapshot config so a concurrent reload cannot mutate it mid-read.
    timeout = _timeout
    ping_count = _ping_count
    default_port = _default_port
    aggregate = _aggregate

    # Budget: stop starting work once it is exhausted, so this callback does
    # not occupy collectd's read thread for most of its Interval.
    #
    # NOT a hard bound. It is checked between operations, and the operations
    # themselves can overrun it: socket.getaddrinfo() takes no timeout
    # argument, so a resolver that hangs for 30s blows an 8s budget no matter
    # what is checked around it. Bounding that would mean resolving in
    # init_cb, caching with a TTL, or running resolution on a thread with a
    # join timeout  -  all of which trade freshness or simplicity for a
    # guarantee this plugin does not currently need.
    deadline = time.monotonic() + _read_budget()

    for _display, hostname, host_port in _hosts:
        port = host_port if host_port is not None else default_port

        # An explicit per-host port has to reach the series name, otherwise
        # "db:5432" and "db:9999" both dispatch as type_instance "db" and
        # overwrite each other. Hosts on the global Port keep the bare
        # hostname, so existing series are unaffected.
        label = hostname if host_port is None else f"{hostname}_{port}"

        # Resolving costs wall-clock too, so honour the deadline before paying
        # for it: getaddrinfo() takes no timeout argument and a stuck resolver
        # would otherwise blow the budget no matter what the connect loop does.
        if time.monotonic() >= deadline:
            collectd.warning(f"{PLUGIN_NAME}: read budget exhausted, skipping '{label}'")
            break

        # Resolved once per read cycle, outside the timed section, and reused
        # for every sample. A resolution failure counts as a full drop.
        addr_info = _resolve(hostname, port)
        if addr_info is None:
            collectd.warning(f"{PLUGIN_NAME}: cannot resolve '{hostname}', counting as drop")

        samples = []
        # Counted separately from ping_count: a probe that was never attempted
        # is not a lost packet. Dividing by ping_count made the plugin's own
        # budget look like packet loss  -  with PingCount 3 and the budget
        # cutting the loop after two successful handshakes, droprate came out
        # at 0.33 with nothing actually dropped, on the one metric people
        # alert on.
        attempts = 0
        for _ in range(ping_count):
            if addr_info is None:
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                collectd.warning(f"{PLUGIN_NAME}: read budget exhausted, skipping remaining probes")
                break
            attempts += 1
            rtt = _tcp_rtt(addr_info, min(timeout, remaining))
            if rtt is not None:
                samples.append(rtt)

        if addr_info is None:
            # Resolution failure is a real drop: the target cannot be reached.
            droprate = 1.0
        elif attempts == 0:
            # Budget gone before a single handshake. Nothing was measured, so
            # claim nothing: NaN leaves a gap rather than inventing loss.
            droprate = float("nan")
        else:
            droprate = (attempts - len(samples)) / attempts

        if samples:
            mean = sum(samples) / len(samples)
            # Reported value: mean by default, minimum when asked for. stddev
            # stays over the whole set either way, so the spread that "min"
            # discards is still visible.
            reported = min(samples) if aggregate == "min" else mean
            if len(samples) > 1:
                variance = sum((r - mean) ** 2 for r in samples) / len(samples)
                stddev = math.sqrt(variance)
            else:
                stddev = float("nan")
        else:
            reported = float("nan")
            stddev = float("nan")

        # type_instance mirrors the ping plugin: bare hostname, plus the port
        # only where one was set per-host (see `label` above).
        _emit(label, "ping", reported)
        _emit(label, "ping_droprate", droprate)
        _emit(label, "ping_stddev", stddev)


collectd.register_config(config_cb)
collectd.register_init(init_cb)
collectd.register_read(read_cb)

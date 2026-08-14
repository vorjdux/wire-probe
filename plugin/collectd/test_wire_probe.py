"""Tests for wire_probe.py, the collectd Python plugin.

Run with:  python3 -m unittest discover -s plugin/collectd -v

The collectd module only exists inside collectd's embedded interpreter, so it
is stubbed here before the plugin is imported. Everything else is real: real
sockets, real getaddrinfo, real read_cb.
"""

import math
import os
import socket
import sys
import threading
import time
import types
import unittest

# --- stub the collectd module ----------------------------------------------
_collectd = types.ModuleType("collectd")
WARNINGS = []
DISPATCHED = []


class _Values:
    def __init__(self):
        self.plugin = None
        self.type = None
        self.type_instance = None
        self.values = []

    def dispatch(self):
        DISPATCHED.append((self.type, self.type_instance, self.values[0]))


_collectd.Values = _Values
_collectd.warning = WARNINGS.append
_collectd.info = lambda _m: None
_collectd.register_config = lambda _f: None
_collectd.register_init = lambda _f: None
_collectd.register_read = lambda _f: None
_collectd.get_interval = lambda: 10.0
sys.modules["collectd"] = _collectd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import wire_probe  # noqa: E402


class Node:
    """Stands in for a collectd config node."""

    def __init__(self, key, values):
        self.key = key
        self.values = values


class Conf:
    def __init__(self, children):
        self.children = children


def listening_socket():
    """A TCP listener that accepts and immediately closes, like the server."""
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(64)

    def serve():
        try:
            while True:
                srv.accept()[0].close()
        except OSError:
            pass

    threading.Thread(target=serve, daemon=True).start()
    return srv, srv.getsockname()[1]


class TestHostParsing(unittest.TestCase):
    def test_forms(self):
        cases = [
            ("host", ("host", None)),
            ("host:1234", ("host", 1234)),
            ("192.0.2.1", ("192.0.2.1", None)),
            ("192.0.2.1:9999", ("192.0.2.1", 9999)),
            # Bare IPv6 must not be split on the last colon: "::1" would
            # become host ":" port 1, a completely different target.
            ("::1", ("::1", None)),
            ("2001:db8::1", ("2001:db8::1", None)),
            ("[::1]", ("::1", None)),
            ("[::1]:9999", ("::1", 9999)),
            ("[2001:db8::1]:80", ("2001:db8::1", 80)),
            # Unusable ports fall back to the global Port.
            ("host:abc", ("host", None)),
            ("host:99999", ("host", None)),
            ("host:0", ("host", None)),
        ]
        for raw, expected in cases:
            with self.subTest(raw=raw):
                self.assertEqual(wire_probe._split_host_port(raw), expected)

    def test_ipv6_forms_actually_resolve(self):
        for raw in ("[::1]:9999", "::1"):
            host, port = wire_probe._split_host_port(raw)
            self.assertIsNotNone(wire_probe._resolve(host, port or 9999), raw)


class TestConfig(unittest.TestCase):
    def setUp(self):
        wire_probe._hosts.clear()
        WARNINGS.clear()

    def test_duplicate_hosts_are_ignored(self):
        wire_probe.config_cb(
            Conf([Node("Host", ["a"]), Node("Host", ["a"]), Node("Host", ["b"])])
        )
        self.assertEqual(len(wire_probe._hosts), 2)
        self.assertTrue(any("duplicate" in w for w in WARNINGS))

    def test_multiple_module_blocks_union(self):
        # collectd calls config_cb once per <Module> block; clearing the list
        # would keep only the last one.
        wire_probe.config_cb(Conf([Node("Host", ["a"])]))
        wire_probe.config_cb(Conf([Node("Host", ["b"])]))
        self.assertEqual(len(wire_probe._hosts), 2)


class TestReadBudget(unittest.TestCase):
    def tearDown(self):
        _collectd.get_interval = lambda: 10.0

    def test_derives_from_the_live_interval(self):
        for interval, expected in ((10.0, 8.0), (1.0, 0.8), (60.0, 48.0)):
            _collectd.get_interval = lambda i=interval: i
            self.assertAlmostEqual(wire_probe._read_budget(), expected)

    def test_falls_back_when_get_interval_is_unusable(self):
        # Older collectd has no get_interval; a broken one may return junk.
        del _collectd.get_interval
        self.assertAlmostEqual(wire_probe._read_budget(), 8.0)
        _collectd.get_interval = lambda: "nonsense"
        self.assertAlmostEqual(wire_probe._read_budget(), 8.0)
        _collectd.get_interval = lambda: 0
        self.assertAlmostEqual(wire_probe._read_budget(), 8.0)

    def test_budget_bounds_a_slow_read(self):
        _collectd.get_interval = lambda: 1.0  # 0.8s budget
        wire_probe._hosts = [("blackhole", "10.255.255.1", 9999)] * 4
        wire_probe._timeout = 5.0
        wire_probe._ping_count = 1
        DISPATCHED.clear()
        t0 = time.monotonic()
        wire_probe.read_cb()
        self.assertLess(time.monotonic() - t0, 3.0)


class TestReadCb(unittest.TestCase):
    def setUp(self):
        self.srv, self.port = listening_socket()
        wire_probe._timeout = 1.0
        wire_probe._ping_count = 1
        wire_probe._default_port = self.port
        DISPATCHED.clear()
        WARNINGS.clear()

    def tearDown(self):
        self.srv.close()

    def values(self):
        return {t: v for t, _ti, v in DISPATCHED}

    def test_reachable_host(self):
        wire_probe._hosts = [("127.0.0.1", "127.0.0.1", None)]
        wire_probe.read_cb()
        v = self.values()
        self.assertEqual(v["ping_droprate"], 0.0)
        self.assertGreater(v["ping"], 0.0)

    def test_closed_port_counts_as_a_drop(self):
        wire_probe._hosts = [("dead", "127.0.0.1", 1)]
        wire_probe.read_cb()
        self.assertEqual(self.values()["ping_droprate"], 1.0)
        self.assertTrue(math.isnan(self.values()["ping"]))

    def test_unresolvable_host_counts_as_a_drop(self):
        wire_probe._hosts = [("nope", "no-such-host.invalid", None)]
        wire_probe.read_cb()
        self.assertEqual(self.values()["ping_droprate"], 1.0)
        self.assertTrue(any("cannot resolve" in w for w in WARNINGS))

    def test_stddev_needs_more_than_one_sample(self):
        wire_probe._hosts = [("127.0.0.1", "127.0.0.1", None)]
        wire_probe._ping_count = 3
        wire_probe.read_cb()
        self.assertGreaterEqual(self.values()["ping_stddev"], 0.0)

    def test_dns_is_not_counted_as_rtt(self):
        # The bug this guards: socket.create_connection() resolves inside the
        # timed section, so a cold lookup was reported as multi-second RTT.
        wire_probe._hosts = [("127.0.0.1", "127.0.0.1", None)]
        real = socket.getaddrinfo

        def slow(*a, **kw):
            time.sleep(1.0)
            return real(*a, **kw)

        socket.getaddrinfo = slow
        try:
            t0 = time.monotonic()
            wire_probe.read_cb()
            wall = time.monotonic() - t0
        finally:
            socket.getaddrinfo = real

        self.assertGreater(wall, 1.0, "the delay was not applied")
        self.assertLess(self.values()["ping"], 50.0, "DNS leaked into the measurement")

    def test_same_host_on_two_ports_gets_distinct_labels(self):
        srv2, port2 = listening_socket()
        try:
            wire_probe._hosts = [
                (f"127.0.0.1:{self.port}", "127.0.0.1", self.port),
                (f"127.0.0.1:{port2}", "127.0.0.1", port2),
            ]
            wire_probe.read_cb()
            labels = {ti for _t, ti, _v in DISPATCHED}
            self.assertEqual(
                labels, {f"127.0.0.1_{self.port}", f"127.0.0.1_{port2}"}
            )
        finally:
            srv2.close()

    def test_budget_truncation_is_not_reported_as_packet_loss(self):
        # Regression guard: dividing drops by PingCount instead of by attempts
        # made the plugin's own budget look like loss. Two handshakes succeed,
        # the third never runs, and droprate must stay 0.
        real_resolve, real_rtt = wire_probe._resolve, wire_probe._tcp_rtt
        _collectd.get_interval = lambda: 1.0  # 0.8s budget

        def slow_success(_addr_info, _timeout):
            time.sleep(0.5)
            return 500.0

        wire_probe._resolve = lambda _h, _p: ("fake", "addr")
        wire_probe._tcp_rtt = slow_success
        wire_probe._hosts = [("h", "h", None)]
        wire_probe._ping_count = 3
        try:
            wire_probe.read_cb()
        finally:
            wire_probe._resolve, wire_probe._tcp_rtt = real_resolve, real_rtt
            _collectd.get_interval = lambda: 10.0
            wire_probe._ping_count = 1

        self.assertEqual(self.values()["ping_droprate"], 0.0)

    def test_nothing_attempted_reports_no_droprate_rather_than_total_loss(self):
        real_resolve, real_rtt = wire_probe._resolve, wire_probe._tcp_rtt
        _collectd.get_interval = lambda: 1.0

        def burn_the_budget(_addr_info, _timeout):
            time.sleep(1.0)
            return 1000.0

        wire_probe._resolve = lambda _h, _p: ("fake", "addr")
        wire_probe._tcp_rtt = burn_the_budget
        # First host eats the budget; the second never gets a handshake.
        wire_probe._hosts = [("a", "a", None), ("b", "b", None)]
        wire_probe._ping_count = 1
        try:
            wire_probe.read_cb()
        finally:
            wire_probe._resolve, wire_probe._tcp_rtt = real_resolve, real_rtt
            _collectd.get_interval = lambda: 10.0

        by_host = {ti: v for t, ti, v in DISPATCHED if t == "ping_droprate"}
        self.assertEqual(by_host.get("a"), 0.0)
        self.assertTrue(
            "b" not in by_host or math.isnan(by_host["b"]),
            f"unattempted host claimed loss: {by_host}",
        )

    def test_aggregate_min_reports_the_least_contaminated_sample(self):
        # One clean handshake among three delayed ones is what a busy CPython
        # looks like: avg is dragged up, min recovers the real number.
        real = wire_probe._tcp_rtt
        rtts = iter([50.0, 0.5, 40.0])
        wire_probe._resolve_backup = wire_probe._resolve
        wire_probe._resolve = lambda _h, _p: ("fake", "addr")
        wire_probe._tcp_rtt = lambda _a, _t: next(rtts)
        wire_probe._hosts = [("h", "h", None)]
        wire_probe._ping_count = 3
        try:
            wire_probe._aggregate = "min"
            wire_probe.read_cb()
            self.assertAlmostEqual(self.values()["ping"], 0.5)
        finally:
            wire_probe._tcp_rtt = real
            wire_probe._resolve = wire_probe._resolve_backup
            wire_probe._aggregate = "avg"
            wire_probe._ping_count = 1

    def test_aggregate_defaults_to_avg_so_existing_series_do_not_move(self):
        real = wire_probe._tcp_rtt
        rtts = iter([50.0, 0.5, 40.0])
        wire_probe._resolve_backup = wire_probe._resolve
        wire_probe._resolve = lambda _h, _p: ("fake", "addr")
        wire_probe._tcp_rtt = lambda _a, _t: next(rtts)
        wire_probe._hosts = [("h", "h", None)]
        wire_probe._ping_count = 3
        try:
            wire_probe.read_cb()
            self.assertAlmostEqual(self.values()["ping"], 30.166666666666668)
        finally:
            wire_probe._tcp_rtt = real
            wire_probe._resolve = wire_probe._resolve_backup
            wire_probe._ping_count = 1

    def test_unknown_aggregate_is_rejected(self):
        wire_probe.config_cb(Conf([Node("Aggregate", ["median"])]))
        self.assertEqual(wire_probe._aggregate, "avg")
        self.assertTrue(any("Aggregate" in w for w in WARNINGS))

    def test_host_on_the_global_port_keeps_the_bare_hostname(self):
        # Existing series must not be renamed.
        wire_probe._hosts = [("127.0.0.1", "127.0.0.1", None)]
        wire_probe.read_cb()
        self.assertEqual({ti for _t, ti, _v in DISPATCHED}, {"127.0.0.1"})


if __name__ == "__main__":
    unittest.main()

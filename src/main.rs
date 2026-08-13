mod cli;
mod exporter;
mod probe;
mod server;

use cli::Mode;
use exporter::Exporter;
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let mode = cli::parse().unwrap_or_else(|e| {
        eprintln!("\nerror: {e}");
        std::process::exit(1);
    });

    match mode {
        Mode::Server { bind, port } => {
            server::run(&bind, port).unwrap_or_else(|e| {
                eprintln!("server error: {e}");
                std::process::exit(1);
            });
        }

        Mode::Probe {
            target,
            target_name,
            az,
            interval,
            timeout,
            export,
        } => run_probe(&target, &target_name, &az, interval, timeout, &export),
    }
}

/// The probe loop, split out of `main` so the mode dispatch stays readable.
fn run_probe(
    target: &str,
    target_name: &str,
    az: &str,
    interval: Duration,
    timeout: Duration,
    export: &cli::ExportDst,
) {
    // Resolve outside the timed section: a per-tick getaddrinfo is a blocking
    // syscall that would corrupt cadence on a slow resolver, and folding it
    // into the measurement reports DNS as RTT.
    let mut addrs = resolve(target).unwrap_or_else(|e| {
        eprintln!("error: cannot resolve '{target}': {e}");
        std::process::exit(1);
    });
    // Index into `addrs`. Exactly one address is probed per cycle, so the
    // reported RTT keeps meaning "handshake with this endpoint"; rotating only
    // on failure is what makes an unreachable first answer (commonly an AAAA
    // ahead of a working A) recoverable.
    let mut addr_idx: usize = 0;

    let host = hostname();
    // Round to nearest whole second; collectd PUTVAL interval= is integer seconds.
    // CLI rejects values > 86400s and max(1.0) guarantees positive; casts are safe.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "CLI caps interval at 86400s and max(1.0) keeps it positive"
    )]
    let interval_secs = interval.as_secs_f64().round().max(1.0) as u32;

    // PUTVAL carries integer seconds, so a sub-second interval tells collectd
    // something the probe is not doing: ten points a second announced as one
    // per second. Legitimate for the UDS path feeding a backend that ignores
    // interval=, wrong for RRD, so warn instead of rejecting.
    if matches!(
        export,
        cli::ExportDst::CollectdExec | cli::ExportDst::CollectdUds(_)
    ) && interval < Duration::from_secs(1)
    {
        eprintln!(
            "warning: --interval {}ms is below 1s, but PUTVAL announces interval={}  -  \
             collectd will be told a rate the probe does not keep",
            interval.as_millis(),
            interval_secs
        );
    }

    let mut exp =
        Exporter::new(export, target_name, az, &host, interval_secs).unwrap_or_else(|e| {
            eprintln!("exporter init error: {e}");
            std::process::exit(1);
        });

    // Latched so a flapping target does not repeat the warning forever.
    let mut overran = false;
    // Counts down to the next re-resolve; reset on every success.
    let mut failures_until_reresolve: u32 = RERESOLVE_AFTER_FAILURES;

    loop {
        let tick = Instant::now();

        // u64 nanoseconds wraps in year ~2554; safe for practical use.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "u64 nanoseconds since the epoch wraps in year ~2554"
        )]
        let ts_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos() as u64;

        // get() rather than indexing: this is the hot loop of a process built
        // with panic = "abort", so an index that ever went stale would kill
        // the agent outright. The invariant is maintained by apply_resolution;
        // this is the belt.
        let addr = if let Some(&a) = addrs.get(addr_idx) {
            a
        } else {
            // Reset and fall through to the first entry rather than
            // `continue`: skipping to the next iteration would skip the sleep
            // too, turning a broken invariant into a hot spin.
            eprintln!("warning: address index went stale, resetting");
            addr_idx = 0;
            let Some(&first) = addrs.first() else {
                eprintln!("error: no addresses left for '{target}'");
                return;
            };
            first
        };
        let sent = match probe::measure_rtt(&addr, timeout) {
            Ok(rtt_ms) => {
                failures_until_reresolve = RERESOLVE_AFTER_FAILURES;
                exp.send(rtt_ms, ts_ns)
            }
            Err(e) => {
                eprintln!("probe error: {e}");
                // Counts down and resets rather than taking a modulo of a
                // running total: no division, and no dependence on the counter
                // never wrapping.
                failures_until_reresolve = failures_until_reresolve.saturating_sub(1);
                if failures_until_reresolve == 0 {
                    failures_until_reresolve = RERESOLVE_AFTER_FAILURES;
                    rotate_target(target, &mut addrs, &mut addr_idx);
                }
                exp.send_failure(ts_ns)
            }
        };
        if let Err(e) = sent {
            // Under the Exec plugin, stdout IS the lifeline: collectd owns the
            // pipe and reads it. Once it is closed, collectd has gone, and
            // nothing this process writes will ever be read again. Exiting lets
            // collectd start a fresh child; looping on would leave an orphan
            // opening TCP connections to the target and reporting to no one.
            // The UDS path reconnects instead, so it is deliberately excluded.
            if e.kind() == std::io::ErrorKind::BrokenPipe
                && matches!(export, cli::ExportDst::CollectdExec)
            {
                eprintln!("stdout closed  -  collectd is gone, exiting");
                return;
            }
            eprintln!("export error: {e}");
        }

        let elapsed = tick.elapsed();
        match interval.checked_sub(elapsed) {
            Some(remaining) => std::thread::sleep(remaining),
            None => {
                // A cycle that outran the interval sets the cadence to
                // `elapsed` instead: a blocking connect cannot be cut short, so
                // the effective rate DROPS to 1/timeout. Warn once so the gap
                // between configured and actual interval is visible in the log.
                if !overran {
                    overran = true;
                    eprintln!(
                        "warning: probe cycle took {}ms, longer than the {}ms interval  -  \
                         cadence is now bounded by --timeout, not --interval",
                        elapsed.as_millis(),
                        interval.as_millis()
                    );
                }
            }
        }
    }
}

/// Refreshes the address list and, when it is unchanged, advances to the next
/// entry.
///
/// Resolving once at startup pins the process to one address for its whole
/// life: a DNS-based failover, or an unreachable AAAA in front of a working A,
/// would never be picked up. Both recover here.
///
/// This runs outside the RTT measurement, but NOT outside the loop:
/// getaddrinfo is synchronous with no timeout of its own, so a stuck resolver
/// stretches one cycle in five. Acceptable because by the time it fires the
/// target is already failing and cadence is already bounded by --timeout, not
/// because it is free.
fn rotate_target(target: &str, addrs: &mut Vec<std::net::SocketAddr>, addr_idx: &mut usize) {
    match resolve(target) {
        Ok(fresh) => match apply_resolution(addrs, addr_idx, fresh) {
            Rotation::Unchanged => {}
            Rotation::NextAddress => {
                if let Some(addr) = addrs.get(*addr_idx) {
                    eprintln!("info: trying the next address for '{target}': {addr}");
                }
            }
            Rotation::Replaced => {
                eprintln!("info: '{target}' now resolves to {}", join_addrs(addrs));
            }
        },
        Err(e) => eprintln!("warning: re-resolving '{target}' failed: {e}"),
    }
}

/// Consecutive probe failures before the target is resolved again.
const RERESOLVE_AFTER_FAILURES: u32 = 5;

/// What a re-resolution did, so the caller can log it and tests can assert it
/// without needing DNS.
#[derive(Debug, PartialEq, Eq)]
enum Rotation {
    /// One address, unchanged: nothing to move to.
    Unchanged,
    /// Same answers, so advance to the next entry in the list.
    NextAddress,
    /// The answers changed; the list was replaced and the index reset.
    Replaced,
}

/// Decides what to do with a fresh resolution. Pure, so the rotation policy is
/// testable without a resolver: DNS is the one part of this that cannot be
/// arranged from a test.
fn apply_resolution(
    addrs: &mut Vec<std::net::SocketAddr>,
    addr_idx: &mut usize,
    fresh: Vec<std::net::SocketAddr>,
) -> Rotation {
    if fresh != *addrs {
        *addrs = fresh;
        *addr_idx = 0;
        return Rotation::Replaced;
    }
    if addrs.len() <= 1 {
        return Rotation::Unchanged;
    }
    // Wrap without `%`: the modulo would be a division clippy cannot prove is
    // non-zero, and this reads the same.
    *addr_idx = addr_idx.saturating_add(1);
    if *addr_idx >= addrs.len() {
        *addr_idx = 0;
    }
    Rotation::NextAddress
}

/// Resolves every address the target currently answers with, in resolver
/// order. Returning all of them is what lets a failing first entry be skipped
/// without a second getaddrinfo.
fn resolve(target: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    let addrs: Vec<_> = target.to_socket_addrs()?.collect();
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no address found for '{target}'"),
        ));
    }
    Ok(addrs)
}

fn join_addrs(addrs: &[std::net::SocketAddr]) -> String {
    addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn hostname() -> String {
    // HOST_NAME_MAX is 255 on Linux; +1 for the null terminator.
    // gethostname(3) truncates to `size` bytes  -  with a zeroed buffer of 256
    // the result is always null-terminated even if the name is exactly 255 chars.
    let mut buf = [0u8; 256];
    // SAFETY: buf is a live, writable 256-byte array and the length passed is
    // its true length, so gethostname cannot write out of bounds.
    unsafe {
        libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    // Sanitise: hostnames injected via orchestrators (k8s, cloud init) may
    // contain slashes, spaces, or other characters that break PUTVAL identifiers.
    // get() over slicing: `end` comes from a position() on this same buffer,
    // so it cannot be out of range, but slicing is the one panic left in a
    // panic = "abort" binary and the fallback costs nothing.
    let name = buf.get(..end).unwrap_or(&buf);
    cli::sanitize(&String::from_utf8_lossy(name))
}

#[cfg(test)]
#[expect(
    clippy::indexing_slicing,
    reason = "tests assert on known-shaped data; a panic here is a test failure, which is the point"
)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_single_address_has_nowhere_to_rotate() {
        let mut addrs = vec![addr("10.0.0.1:9999")];
        let mut idx = 0;
        let same = addrs.clone();
        let r = apply_resolution(&mut addrs, &mut idx, same);
        assert_eq!(r, Rotation::Unchanged);
        assert_eq!(idx, 0);
    }

    #[test]
    fn an_unreachable_first_answer_is_skipped_on_the_next_round() {
        // The case this exists for: DNS keeps returning an AAAA that never
        // connects, ahead of an A that does. Without rotation the probe stays
        // pinned to the dead one forever.
        let mut addrs = vec![addr("[::1]:9999"), addr("10.0.0.1:9999")];
        let mut idx = 0;

        let same = addrs.clone();
        assert_eq!(
            apply_resolution(&mut addrs, &mut idx, same.clone()),
            Rotation::NextAddress
        );
        assert_eq!(addrs[idx], addr("10.0.0.1:9999"));

        // And it wraps back rather than running off the end.
        assert_eq!(
            apply_resolution(&mut addrs, &mut idx, same),
            Rotation::NextAddress
        );
        assert_eq!(addrs[idx], addr("[::1]:9999"));
    }

    #[test]
    fn changed_answers_replace_the_list_and_reset_the_index() {
        let mut addrs = vec![addr("10.0.0.1:9999"), addr("10.0.0.2:9999")];
        let mut idx = 1;
        let r = apply_resolution(&mut addrs, &mut idx, vec![addr("10.0.0.9:9999")]);
        assert_eq!(r, Rotation::Replaced);
        assert_eq!(addrs, vec![addr("10.0.0.9:9999")]);
        assert_eq!(idx, 0, "a stale index would panic on the shorter list");
    }

    #[test]
    fn resolve_returns_every_answer() {
        let addrs = resolve("localhost:9999").unwrap();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.port() == 9999));
    }

    #[test]
    fn resolve_reports_an_unknown_host_instead_of_returning_nothing() {
        assert!(resolve("no-such-host.invalid:9999").is_err());
    }
}

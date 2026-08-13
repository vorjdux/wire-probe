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
        } => {
            // Resolve once at startup. Per-tick getaddrinfo would run a blocking
            // syscall every interval and corrupt cadence on slow/failing resolvers.
            let addr = target
                .to_socket_addrs()
                .unwrap_or_else(|e| {
                    eprintln!("error: cannot resolve '{target}': {e}");
                    std::process::exit(1);
                })
                .next()
                .unwrap_or_else(|| {
                    eprintln!("error: no address found for '{target}'");
                    std::process::exit(1);
                });

            let host = hostname();
            // Round to nearest whole second; collectd PUTVAL interval= is integer seconds.
            // CLI rejects values > 86400s and max(1.0) guarantees positive; casts are safe.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let interval_secs = interval.as_secs_f64().round().max(1.0) as u32;

            let mut exp = Exporter::new(&export, &target_name, &az, &host, interval_secs)
                .unwrap_or_else(|e| {
                    eprintln!("exporter init error: {e}");
                    std::process::exit(1);
                });

            // Latched so a flapping target does not repeat the warning forever.
            let mut overran = false;

            loop {
                let tick = Instant::now();

                // u64 nanoseconds wraps in year ~2554; safe for practical use.
                #[allow(clippy::cast_possible_truncation)]
                let ts_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO)
                    .as_nanos() as u64;

                let sent = match probe::measure_rtt(&addr, timeout) {
                    Ok(rtt_ms) => exp.send(rtt_ms, ts_ns),
                    Err(e) => {
                        eprintln!("probe error: {e}");
                        exp.send_failure(ts_ns)
                    }
                };
                if let Err(e) = sent {
                    eprintln!("export error: {e}");
                }

                let elapsed = tick.elapsed();
                match interval.checked_sub(elapsed) {
                    Some(remaining) => std::thread::sleep(remaining),
                    None => {
                        // A cycle that outran the interval sets the cadence to
                        // `elapsed` instead: a blocking connect cannot be cut
                        // short, so the effective rate DROPS to 1/timeout. Warn
                        // once so the gap between configured and actual
                        // interval is visible in the log.
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
    }
}

fn hostname() -> String {
    // HOST_NAME_MAX is 255 on Linux; +1 for the null terminator.
    // gethostname(3) truncates to `size` bytes  -  with a zeroed buffer of 256
    // the result is always null-terminated even if the name is exactly 255 chars.
    let mut buf = [0u8; 256];
    unsafe {
        libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    // Sanitise: hostnames injected via orchestrators (k8s, cloud init) may
    // contain slashes, spaces, or other characters that break PUTVAL identifiers.
    cli::sanitize(&String::from_utf8_lossy(&buf[..end]))
}

mod cli;
mod exporter;
mod probe;
mod server;

use cli::Mode;
use exporter::Exporter;
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
            let host = hostname();
            // Round to nearest whole second; collectd PUTVAL interval= is integer seconds.
            // The CLI already rejects values > 86400s so the cast is safe.
            let interval_secs = interval.as_secs_f64().round().max(1.0) as u32;

            let mut exp =
                Exporter::new(&export, &target_name, &az, &host, interval_secs).unwrap_or_else(
                    |e| {
                        eprintln!("exporter init error: {e}");
                        std::process::exit(1);
                    },
                );

            loop {
                let tick = Instant::now();

                match probe::measure_rtt(&target, timeout) {
                    Ok(rtt_ms) => {
                        let ts_ns = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or(Duration::ZERO)
                            .as_nanos() as u64;
                        if let Err(e) = exp.send(rtt_ms, ts_ns) {
                            eprintln!("export error: {e}");
                        }
                    }
                    Err(e) => eprintln!("probe error: {e}"),
                }

                let elapsed = tick.elapsed();
                if elapsed < interval {
                    std::thread::sleep(interval - elapsed);
                }
            }
        }
    }
}

fn hostname() -> String {
    // HOST_NAME_MAX is 255 on Linux; +1 for the null terminator.
    // gethostname(3) truncates to `size` bytes — with a zeroed buffer of 256
    // the result is always null-terminated even if the name is exactly 255 chars.
    let mut buf = [0u8; 256];
    unsafe {
        libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    // Sanitise: hostnames injected via orchestrators (k8s, cloud init) may
    // contain slashes, spaces, or other characters that break PUTVAL identifiers.
    cli::sanitize(&String::from_utf8_lossy(&buf[..end]))
}

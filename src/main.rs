mod cli;
mod exporter;
mod probe;
mod server;

use cli::Mode;
use exporter::Exporter;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let mode = cli::parse().unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    match mode {
        Mode::Server { port } => {
            server::run(port).unwrap_or_else(|e| {
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
            let interval_secs = interval.as_secs().max(1) as u32;

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
    let mut buf = [0u8; 64];
    unsafe {
        libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

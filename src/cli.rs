use pico_args::Arguments;
use std::ffi::OsString;
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub enum ExportDst {
    TelegrafUdp(String),
    CollectdUds(String),
    CollectdExec,
}

#[derive(Debug)]
pub enum Mode {
    Server {
        bind: String,
        port: u16,
    },
    Probe {
        target: String,
        target_name: String,
        az: String,
        interval: Duration,
        timeout: Duration,
        export: ExportDst,
    },
}

/// Minimum probe interval (100 ms). Prevents self-DoS via a zero/tiny interval.
const MIN_INTERVAL_MS: u64 = 100;
/// Maximum probe interval (24 h). Prevents u32 overflow in `interval_secs`.
const MAX_INTERVAL_MS: u64 = 86_400_000;
/// Maximum connect timeout (60 s). Prevents probes from hanging indefinitely.
const MAX_TIMEOUT_MS: u64 = 60_000;

pub fn print_help() {
    eprintln!(
        "wire-probe {VERSION}  -  zero-footprint L4 TCP telemetry agent
Author: Matheus Santos <vorj.dux@gmail.com>
License: MIT  |  https://github.com/vorjdux/wire-probe

USAGE:
    wire-probe --mode <server|probe> [OPTIONS]

MODES:
  server    Accept/drop loop on the target host (io_uring, ~500 KB RSS)
  probe     Measure TCP handshake RTT and export metrics

SERVER OPTIONS:
  --port <port>       TCP port to listen on          [default: 9999]
  --bind <addr>       Bind address                   [default: 0.0.0.0]

PROBE OPTIONS:
  --target <host:port>      wire-probe server address (required)
  --target-name <name>      Label for metric names    [default: derived from --target]
  --az <zone>               Availability-zone tag     [default: default]
  --interval <duration>     Time between probes       [default: 1000ms, min: 100ms, max: 24h]
  --timeout <duration>      Connect timeout per probe [default: 5000ms, max: 60s]
  --export <dst>            Export destination        [default: collectd-exec]

EXPORT DESTINATIONS:
  collectd-exec                     Write PUTVAL lines to stdout
  collectd-uds://<path>             Write PUTVAL lines to a Unix socket
  telegraf-udp://<host>:<port>      Send Influx Line Protocol over UDP

DURATION FORMAT:
  Suffix with 'ms' for milliseconds or 's' for seconds (e.g. 500ms, 10s)

EXAMPLES:
  wire-probe --mode server --port 9999
  wire-probe --mode probe --target db-host:9999 --export telegraf-udp://127.0.0.1:8094
  wire-probe --mode probe --target db-host:9999 --interval 10s --export collectd-exec"
    );
}

pub fn parse() -> Result<Mode, Box<dyn std::error::Error>> {
    let mut args = Arguments::from_env();

    if args.contains(["-h", "--help"]) {
        print_help();
        std::process::exit(0);
    }

    if args.contains("--version") {
        eprintln!("wire-probe {VERSION}");
        std::process::exit(0);
    }

    let mode: String = args.opt_value_from_str("--mode")?.ok_or_else(|| {
        print_help();
        "missing required flag --mode"
    })?;

    match mode.as_str() {
        "server" => {
            let bind: String = args
                .opt_value_from_str("--bind")?
                .unwrap_or_else(|| "0.0.0.0".to_string());
            let port: u16 = args.opt_value_from_str("--port")?.unwrap_or(9999);
            reject_unknown(args.finish())?;
            Ok(Mode::Server { bind, port })
        }
        "probe" => {
            let target: String = args.value_from_str("--target")?;
            let target_name: String = args
                .opt_value_from_str("--target-name")?
                .map_or_else(|| sanitize(&target), |s: String| sanitize(&s));
            let az: String = args
                .opt_value_from_str("--az")?
                .map_or_else(|| "default".to_string(), |s: String| sanitize(&s));
            let interval_str: String = args
                .opt_value_from_str("--interval")?
                .unwrap_or_else(|| "1000ms".to_string());
            let timeout_str: String = args
                .opt_value_from_str("--timeout")?
                .unwrap_or_else(|| "5000ms".to_string());
            let export_str: String = args
                .opt_value_from_str("--export")?
                .unwrap_or_else(|| "collectd-exec".to_string());

            let interval = parse_duration(&interval_str)?;
            if interval.as_millis() < u128::from(MIN_INTERVAL_MS) {
                return Err(format!("interval too small (min {MIN_INTERVAL_MS}ms)").into());
            }
            if interval.as_millis() > u128::from(MAX_INTERVAL_MS) {
                return Err(format!("interval too large (max {MAX_INTERVAL_MS}ms = 24h)").into());
            }
            let timeout = parse_duration(&timeout_str)?;
            if timeout.is_zero() {
                return Err("timeout must be greater than zero".into());
            }
            if timeout.as_millis() > u128::from(MAX_TIMEOUT_MS) {
                return Err(format!("timeout too large (max {MAX_TIMEOUT_MS}ms = 60s)").into());
            }
            // Not an error: a long timeout is legitimate on a high-latency link.
            // But a probe that blocks past its own interval silently drops to a
            // cadence of 1/timeout, so say so instead of letting it be inferred
            // from thinning data.
            if timeout > interval {
                eprintln!(
                    "warning: --timeout ({}ms) exceeds --interval ({}ms)  -  a failing target \
                     will be probed every {}ms, not every {}ms",
                    timeout.as_millis(),
                    interval.as_millis(),
                    timeout.as_millis(),
                    interval.as_millis()
                );
            }
            let export = parse_export(&export_str)?;
            reject_unknown(args.finish())?;

            Ok(Mode::Probe {
                target,
                target_name,
                az,
                interval,
                timeout,
                export,
            })
        }
        other => Err(format!("unknown mode '{other}'; expected 'server' or 'probe'").into()),
    }
}

fn reject_unknown(leftover: Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(arg) = leftover.into_iter().next() {
        return Err(format!("unexpected argument: {}", arg.to_string_lossy()).into());
    }
    Ok(())
}

/// Strips characters that are unsafe in ILP tag values and PUTVAL identifiers.
/// Keeps ASCII alphanumeric, hyphen, and dot; replaces everything else with `_`.
/// ASCII-only: ILP and PUTVAL parsers are ASCII-based; raw UTF-8 multi-byte
/// sequences would corrupt the byte stream.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_duration(s: &str) -> Result<Duration, Box<dyn std::error::Error>> {
    if let Some(ms) = s.strip_suffix("ms") {
        Ok(Duration::from_millis(ms.parse()?))
    } else if let Some(sec) = s.strip_suffix('s') {
        Ok(Duration::from_secs(sec.parse()?))
    } else {
        Ok(Duration::from_millis(s.parse()?))
    }
}

#[allow(clippy::option_if_let_else)]
fn parse_export(s: &str) -> Result<ExportDst, Box<dyn std::error::Error>> {
    if let Some(addr) = s.strip_prefix("telegraf-udp://") {
        Ok(ExportDst::TelegrafUdp(addr.to_string()))
    } else if let Some(path) = s.strip_prefix("collectd-uds://") {
        Ok(ExportDst::CollectdUds(path.to_string()))
    } else if s == "collectd-exec" {
        Ok(ExportDst::CollectdExec)
    } else {
        Err(format!(
            "unknown export scheme '{s}'; expected telegraf-udp://<addr>, collectd-uds://<path>, or collectd-exec"
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_chars_and_replaces_the_rest() {
        assert_eq!(sanitize("db-node-01.eu"), "db-node-01.eu");
        // Characters that would break ILP tag values or PUTVAL identifiers.
        assert_eq!(sanitize("a b/c,d=e:f"), "a_b_c_d_e_f");
        // ASCII-only: multi-byte UTF-8 must not survive as raw bytes.
        assert_eq!(sanitize("café"), "caf_");
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn parse_duration_ms_suffix_wins_over_s() {
        // Regression guard: strip_suffix("ms") MUST be tried before
        // strip_suffix('s'). Reversed, "500ms" parses as "500m" seconds and
        // the whole probe cadence silently changes by 1000x.
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        // Bare number means milliseconds.
        assert_eq!(parse_duration("250").unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("-5s").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn parse_export_recognises_every_scheme() {
        assert!(matches!(
            parse_export("collectd-exec").unwrap(),
            ExportDst::CollectdExec
        ));
        match parse_export("telegraf-udp://127.0.0.1:8094").unwrap() {
            ExportDst::TelegrafUdp(a) => assert_eq!(a, "127.0.0.1:8094"),
            other => panic!("wrong variant: {other:?}"),
        }
        match parse_export("collectd-uds:///var/run/collectd-unixsock").unwrap() {
            ExportDst::CollectdUds(p) => assert_eq!(p, "/var/run/collectd-unixsock"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_export_rejects_unknown_scheme() {
        assert!(parse_export("kafka://localhost:9092").is_err());
        assert!(parse_export("").is_err());
        // Near-misses on the known schemes must not fall through.
        assert!(parse_export("collectd-exe").is_err());
        assert!(parse_export("telegraf-udp:/127.0.0.1:8094").is_err());
    }

    #[test]
    fn reject_unknown_flags_a_leftover_argument() {
        assert!(reject_unknown(vec![]).is_ok());
        assert!(reject_unknown(vec![OsString::from("--typoed")]).is_err());
    }
}

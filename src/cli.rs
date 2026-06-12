use std::ffi::OsString;
use std::time::Duration;
use pico_args::Arguments;

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

/// Maximum allowed probe interval (24 h). Prevents u32 overflow in `interval_secs`.
const MAX_INTERVAL_MS: u64 = 86_400_000;
/// Maximum connect timeout (60 s). Prevents probes from hanging indefinitely.
const MAX_TIMEOUT_MS: u64 = 60_000;

pub fn print_help() {
    eprintln!(
        "wire-probe {VERSION} — zero-footprint L4 TCP telemetry agent
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
  --interval <duration>     Time between probes       [default: 1000ms]
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

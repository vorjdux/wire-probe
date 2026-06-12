use std::ffi::OsString;
use std::time::Duration;
use pico_args::Arguments;

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

/// Maximum allowed probe interval (24 h). Prevents u32 overflow in interval_secs.
const MAX_INTERVAL_MS: u64 = 86_400_000;

pub fn parse() -> Result<Mode, Box<dyn std::error::Error>> {
    let mut args = Arguments::from_env();
    let mode: String = args.value_from_str("--mode")?;

    match mode.as_str() {
        "server" => {
            let bind: String = args
                .opt_value_from_str("--bind")?
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let port: u16 = args.opt_value_from_str("--port")?.unwrap_or(9999);
            reject_unknown(args.finish())?;
            Ok(Mode::Server { bind, port })
        }
        "probe" => {
            let target: String = args.value_from_str("--target")?;
            let target_name: String = args
                .opt_value_from_str("--target-name")?
                .map(|s: String| sanitize(&s))
                .unwrap_or_else(|| sanitize(&target));
            let az: String = args
                .opt_value_from_str("--az")?
                .map(|s: String| sanitize(&s))
                .unwrap_or_else(|| "default".to_string());
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
            if interval.as_millis() as u64 > MAX_INTERVAL_MS {
                return Err(format!(
                    "interval too large (max {}ms = 24h)",
                    MAX_INTERVAL_MS
                )
                .into());
            }
            let timeout = parse_duration(&timeout_str)?;
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
/// Keeps alphanumeric, hyphen, and dot; replaces everything else with `_`.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' {
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

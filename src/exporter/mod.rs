mod collectd;
mod telegraf;

use crate::cli::ExportDst;
use std::io;

pub enum Exporter {
    Telegraf(telegraf::TelegrafExporter),
    Collectd(collectd::CollectdExporter),
}

impl Exporter {
    pub fn new(
        dst: &ExportDst,
        target_name: &str,
        az: &str,
        hostname: &str,
        interval_secs: u32,
    ) -> io::Result<Self> {
        match dst {
            ExportDst::TelegrafUdp(addr) => Ok(Exporter::Telegraf(
                telegraf::TelegrafExporter::new(addr, target_name, az)?,
            )),
            ExportDst::CollectdUds(path) => Ok(Exporter::Collectd(
                collectd::CollectdExporter::new_uds(path, hostname, target_name, interval_secs)?,
            )),
            ExportDst::CollectdExec => Ok(Exporter::Collectd(
                collectd::CollectdExporter::new_exec(hostname, target_name, interval_secs),
            )),
        }
    }

    pub fn send(&mut self, rtt_ms: f64, ts_ns: u64) -> io::Result<()> {
        match self {
            Exporter::Telegraf(e) => e.send(rtt_ms, ts_ns),
            Exporter::Collectd(e) => e.send(rtt_ms),
        }
    }
}

//! The `process_*` family (prometheus-net parity) plus the `ferrofin_tokio_*`
//! gauges — all observable instruments whose sync callbacks read cheap
//! `/proc/self` files (or the tokio runtime metrics) on each scrape.
//!
//! `/proc`-sourced instruments are Linux-only (the deploy target); on other
//! platforms they are simply not registered. The parsers are pure functions so
//! they can be unit-tested without a real `/proc`.
//!
// This module is all numeric-cast metric emission (kernel ticks/pages → gauge
// values). The precision/truncation/sign casts are inherent and benign at these
// magnitudes, so the pedantic cast lints are allowed module-wide.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use opentelemetry::metrics::Meter;
use tokio::runtime::Handle;

/// Registers the process + tokio instruments on `meter`. `runtime` is captured
/// so the tokio-metric callbacks read the server's runtime (they may not call
/// `Handle::current()` — a scrape can happen off a runtime thread).
pub(crate) fn register(meter: &Meter, runtime: Handle) {
    register_tokio(meter, runtime);
    #[cfg(target_os = "linux")]
    register_proc(meter);
}

/// `ferrofin_tokio_*` — the honest analogue of `dotnet_threadpool_*`.
fn register_tokio(meter: &Meter, runtime: Handle) {
    let workers = runtime.clone();
    meter
        .u64_observable_gauge("ferrofin_tokio_workers")
        .with_description("Number of worker threads in the tokio runtime.")
        .with_callback(move |obs| {
            obs.observe(workers.metrics().num_workers() as u64, &[]);
        })
        .build();
    meter
        .u64_observable_gauge("ferrofin_tokio_alive_tasks")
        .with_description("Number of currently alive tasks in the tokio runtime.")
        .with_callback(move |obs| {
            obs.observe(runtime.metrics().num_alive_tasks() as u64, &[]);
        })
        .build();

    // Cross-platform, no /proc needed.
    meter
        .u64_observable_gauge("process_cpu_count")
        .with_description("The number of processor cores available to this process.")
        .with_callback(|obs| {
            let cores = std::thread::available_parallelism().map_or(0, |n| n.get() as u64);
            obs.observe(cores, &[]);
        })
        .build();
}

#[cfg(target_os = "linux")]
fn register_proc(meter: &Meter) {
    let clk_tck = clock_ticks_per_sec();
    let page = page_size();

    meter
        .f64_observable_counter("process_cpu_seconds_total")
        .with_description("Total user and system CPU time spent in seconds.")
        .with_callback(move |obs| {
            if let Some(ticks) = read_stat().and_then(|s| parse_stat_cpu_ticks(&s)) {
                obs.observe(ticks as f64 / clk_tck, &[]);
            }
        })
        .build();
    meter
        .f64_observable_gauge("process_start_time_seconds")
        .with_description("Start time of the process since unix epoch in seconds.")
        .with_callback(move |obs| {
            if let Some(secs) = process_start_time_seconds(clk_tck) {
                obs.observe(secs, &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("process_working_set_bytes")
        .with_description("Process working set (resident) memory in bytes.")
        .with_callback(move |obs| {
            if let Some(pages) = read_statm().and_then(|s| parse_statm_field(&s, 1)) {
                obs.observe(pages * page, &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("process_virtual_memory_bytes")
        .with_description("Process virtual memory size in bytes.")
        .with_callback(move |obs| {
            if let Some(pages) = read_statm().and_then(|s| parse_statm_field(&s, 0)) {
                obs.observe(pages * page, &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("process_private_memory_bytes")
        .with_description("Process private (anonymous resident) memory in bytes.")
        .with_callback(|obs| {
            if let Some(kib) = read_status().and_then(|s| parse_status_rss_anon_kib(&s)) {
                obs.observe(kib * 1024, &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("process_num_threads")
        .with_description("The number of OS threads in the process.")
        .with_callback(|obs| {
            if let Some(n) = read_stat().and_then(|s| parse_stat_num_threads(&s)) {
                obs.observe(n, &[]);
            }
        })
        .build();
    meter
        .u64_observable_gauge("process_open_handles")
        .with_description("The number of open file descriptors.")
        .with_callback(|obs| {
            if let Some(n) = open_fd_count() {
                obs.observe(n, &[]);
            }
        })
        .build();
}

// ── sysconf ─────────────────────────────────────────────────────────────────

/// `sysconf(_SC_CLK_TCK)` — kernel ticks per second (usually 100). Falls back to
/// 100 if the query fails.
#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> f64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as f64 } else { 100.0 }
}

/// `sysconf(_SC_PAGESIZE)` — bytes per memory page. Falls back to 4096.
#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u64 } else { 4096 }
}

// ── /proc readers (thin I/O wrappers over the pure parsers) ──────────────────

#[cfg(target_os = "linux")]
fn read_stat() -> Option<String> {
    std::fs::read_to_string("/proc/self/stat").ok()
}
#[cfg(target_os = "linux")]
fn read_statm() -> Option<String> {
    std::fs::read_to_string("/proc/self/statm").ok()
}
#[cfg(target_os = "linux")]
fn read_status() -> Option<String> {
    std::fs::read_to_string("/proc/self/status").ok()
}
#[cfg(target_os = "linux")]
fn open_fd_count() -> Option<u64> {
    let n = std::fs::read_dir("/proc/self/fd").ok()?.count();
    u64::try_from(n).ok()
}

#[cfg(target_os = "linux")]
fn process_start_time_seconds(clk_tck: f64) -> Option<f64> {
    let starttime = read_stat().and_then(|s| parse_stat_starttime(&s))?;
    let btime = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| parse_btime(&s))?;
    Some(starttime as f64 / clk_tck + btime as f64)
}

// ── pure parsers (unit-tested) ───────────────────────────────────────────────

/// Returns the whitespace-split fields of `/proc/self/stat` AFTER the process
/// name — i.e. starting at field 3 (`state`). Splitting after the LAST `)`
/// survives a `comm` containing spaces or parentheses.
fn stat_fields_after_comm(stat: &str) -> Option<Vec<&str>> {
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().collect())
}

/// `utime + stime` in clock ticks (`/proc/self/stat` fields 14 + 15).
fn parse_stat_cpu_ticks(stat: &str) -> Option<u64> {
    let f = stat_fields_after_comm(stat)?; // f[0] == field 3 (state)
    let utime: u64 = f.get(11)?.parse().ok()?; // field 14
    let stime: u64 = f.get(12)?.parse().ok()?; // field 15
    Some(utime + stime)
}

/// Thread count (`/proc/self/stat` field 20).
fn parse_stat_num_threads(stat: &str) -> Option<u64> {
    stat_fields_after_comm(stat)?.get(17)?.parse().ok() // field 20
}

/// Process start time in clock ticks since boot (`/proc/self/stat` field 22).
fn parse_stat_starttime(stat: &str) -> Option<u64> {
    stat_fields_after_comm(stat)?.get(19)?.parse().ok() // field 22
}

/// The `n`-th (0-based) space-separated field of `/proc/self/statm`, in pages.
fn parse_statm_field(statm: &str, n: usize) -> Option<u64> {
    statm.split_whitespace().nth(n)?.parse().ok()
}

/// `RssAnon:` from `/proc/self/status`, in kibibytes.
fn parse_status_rss_anon_kib(status: &str) -> Option<u64> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// The boot time (`btime` line) from `/proc/stat`, in seconds since the epoch.
fn parse_btime(stat: &str) -> Option<u64> {
    for line in stat.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // A stat line whose comm contains spaces AND a `)` — the adversarial case.
    const STAT: &str = "1234 (weird ) name) S 1 1234 1234 0 -1 4194304 \
        100 0 0 0 42 8 0 0 20 0 7 0 999 123456 456";

    #[test]
    fn parses_cpu_ticks_past_a_parenthesised_comm() {
        // utime=42 (field 14) + stime=8 (field 15) = 50.
        assert_eq!(parse_stat_cpu_ticks(STAT), Some(50));
    }

    #[test]
    fn parses_num_threads_and_starttime() {
        assert_eq!(parse_stat_num_threads(STAT), Some(7)); // field 20
        assert_eq!(parse_stat_starttime(STAT), Some(999)); // field 22
    }

    #[test]
    fn parses_statm_fields() {
        assert_eq!(parse_statm_field("1000 250 40 5 0 300 0", 0), Some(1000));
        assert_eq!(parse_statm_field("1000 250 40 5 0 300 0", 1), Some(250));
    }

    #[test]
    fn parses_rss_anon_and_btime() {
        let status = "Name:\tferrofin\nVmRSS:\t  4096 kB\nRssAnon:\t  2048 kB\n";
        assert_eq!(parse_status_rss_anon_kib(status), Some(2048));
        assert_eq!(
            parse_btime("cpu 0 0\nbtime 1700000000\nprocesses 5"),
            Some(1_700_000_000)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_proc_reads_are_sane() {
        // Smoke: the real /proc/self is readable and non-degenerate.
        assert!(open_fd_count().unwrap() > 0);
        assert!(parse_stat_num_threads(&read_stat().unwrap()).unwrap() >= 1);
    }
}

//! The server's daily log file, named as Jellyfin names it.
//!
//! Jellyfin's Serilog file sink writes `%JELLYFIN_LOG_DIR%//log_.log` with
//! `"rollingInterval": "Day"` (`Jellyfin.Server/Resources/Configuration/logging.json`),
//! so each day's file is `log_yyyyMMdd.log`, dated by local time (Serilog's
//! `DateTime.Now`), and on each new file it deletes all but the newest
//! `retainedFileCountLimit` (3) of its own files. `tracing_appender`'s rolling
//! file can only write `prefix.yyyy-MM-dd.suffix` by the UTC date, so this is a
//! writer of its own. Serilog's size roll (`rollOnFileSizeLimit`) is not ported
//! (gl-1832).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDate};

/// The day's server log in `dir`, rolled to a new file at local midnight.
pub(crate) struct DailyLogFile {
    dir: PathBuf,
    /// How many of the daily files to keep, the current one included.
    keep: usize,
    day: NaiveDate,
    file: File,
}

impl DailyLogFile {
    /// Opens (appends to) today's file in `dir`, then deletes all but the
    /// newest `keep - 1` other daily files.
    pub(crate) fn open(dir: &Path, keep: usize) -> io::Result<Self> {
        let day = Local::now().date_naive();
        Ok(Self {
            dir: dir.to_path_buf(),
            keep,
            day,
            file: open_day(dir, day, keep)?,
        })
    }

    /// Moves to `today`'s file once a later day has begun; a clock set back
    /// never moves it back. A file that cannot be opened leaves the writes
    /// going to the last day's until the next day, as `tracing_appender`
    /// tries once per rollover.
    fn roll_to(&mut self, today: NaiveDate) {
        if today <= self.day {
            return;
        }
        self.day = today;
        match open_day(&self.dir, today, self.keep) {
            Ok(file) => self.file = file,
            // The log cannot report its own failure; stderr reaches the journal.
            Err(e) => eprintln!("cannot open the log file for {today}: {e}"),
        }
    }
}

impl io::Write for DailyLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.roll_to(Local::now().date_naive());
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Serilog's name for `day`'s file.
fn file_name(day: NaiveDate) -> String {
    format!("log_{}.log", day.format("%Y%m%d"))
}

/// Opens `day`'s file for appending, then applies the retention (in
/// Serilog's order, so a failed open deletes nothing).
fn open_day(dir: &Path, day: NaiveDate, keep: usize) -> io::Result<File> {
    let current = file_name(day);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(&current))?;
    prune(dir, &current, keep);
    Ok(file)
}

/// Serilog's `ApplyRetentionPolicy` with a `retainedFileCountLimit` of `keep`:
/// of the daily files other than `current`, newest first by date and then by
/// size-roll sequence, all but the first `keep - 1` are deleted.
fn prune(dir: &Path, current: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<((u32, u32), PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name == current {
                return None;
            }
            Some((daily_rank(name)?, entry.path()))
        })
        .collect();
    // Newest first; the path settles a tie between the two name families.
    files.sort_unstable();
    files.reverse();
    for (_, path) in files.into_iter().skip(keep.saturating_sub(1)) {
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("cannot delete the old log file {}: {e}", path.display());
        }
    }
}

/// The date and size-roll sequence of a Serilog daily file name,
/// `log_yyyyMMdd.log` or `log_yyyyMMdd_NNN.log` (Serilog's `PathRoller`
/// pattern: eight digits, then optionally `_` and three or more), or of the
/// `log.yyyy-MM-dd.log` an earlier Ferrofin build wrote, so the count covers
/// both. Eight digits that are no date rank oldest, as Serilog's unparsed
/// period sorts last. `None` for any other name, which is never pruned here.
fn daily_rank(name: &str) -> Option<(u32, u32)> {
    if let Some(date) = name
        .strip_prefix("log.")
        .and_then(|rest| rest.strip_suffix(".log"))
    {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
        return Some((date.format("%Y%m%d").to_string().parse().ok()?, 0));
    }
    let stem = name.strip_prefix("log_")?.strip_suffix(".log")?;
    let (date, sequence) = stem.split_at_checked(8)?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(date) {
        return None;
    }
    let date = if NaiveDate::parse_from_str(date, "%Y%m%d").is_ok() {
        date.parse().ok()?
    } else {
        0
    };
    let sequence = match sequence.strip_prefix('_') {
        None if sequence.is_empty() => 0,
        Some(n) if n.len() >= 3 && digits(n) => n.parse().ok()?,
        _ => return None,
    };
    Some((date, sequence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_file_is_named_by_the_day_as_serilog_names_it() {
        assert_eq!(file_name(day(2026, 10, 23)), "log_20261023.log");
    }

    #[rstest::rstest]
    #[case::daily("log_20261023.log", Some((20_261_023, 0)))]
    #[case::size_rolled("log_20261023_001.log", Some((20_261_023, 1)))]
    #[case::long_sequence("log_20261023_1000.log", Some((20_261_023, 1000)))]
    #[case::short_sequence("log_20261023_01.log", None)]
    #[case::earlier_build("log.2026-10-23.log", Some((20_261_023, 0)))]
    #[case::earlier_build_not_a_date("log.2026-13-23.log", None)]
    #[case::short_date("log_2026102.log", None)]
    #[case::not_a_date("log_20261399.log", Some((0, 0)))]
    #[case::not_digits("log_2026102x.log", None)]
    #[case::other_suffix("log_20261023.txt", None)]
    #[case::ffmpeg("FFmpeg.Remux-2026-10-23_22-06-33__46340374.log", None)]
    fn only_daily_log_names_rank(#[case] name: &str, #[case] rank: Option<(u32, u32)>) {
        assert_eq!(daily_rank(name), rank);
    }

    /// Retention keeps the newest daily files, Jellyfin's own (an adopted
    /// install's) and an earlier build's `log.yyyy-MM-dd.log` counted with
    /// them, and never touches another name.
    #[test]
    fn retention_keeps_the_newest_daily_files_and_no_other_name() {
        let tmp = tempfile::tempdir().unwrap();
        for name in [
            "log_20261020.log",
            "log_20261021.log",
            "log_20261021_001.log",
            "log_20261022.log",
            "log.2026-10-19.log",
            "log.2026-10-22.log",
            "log_notes.log",
            "FFmpeg.Transcode-2026-10-22_10-00-00__0a1b2c3d.log",
        ] {
            std::fs::write(tmp.path().join(name), name).unwrap();
        }
        prune(tmp.path(), "log_20261023.log", 3);
        assert_eq!(
            names(tmp.path()),
            [
                "FFmpeg.Transcode-2026-10-22_10-00-00__0a1b2c3d.log",
                "log.2026-10-22.log",
                "log_20261022.log",
                "log_notes.log",
            ]
        );
    }

    /// A `keep` of 0 or 1 keeps only the current file.
    #[rstest::rstest]
    fn keeping_one_file_keeps_only_the_current_one(#[values(0, 1)] keep: usize) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("log_20261021.log"), "old").unwrap();
        prune(tmp.path(), "log_20261023.log", keep);
        assert_eq!(names(tmp.path()), Vec::<String>::new());
    }

    /// At a new day the writes go to that day's file, appended after anything
    /// the file already held.
    #[test]
    fn a_new_day_writes_to_its_own_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("log_20261024.log"), "earlier\n").unwrap();
        let mut log = DailyLogFile::open(tmp.path(), 10).unwrap();
        log.day = day(2026, 10, 22);
        log.roll_to(day(2026, 10, 23));
        log.file.write_all(b"first\n").unwrap();
        log.roll_to(day(2026, 10, 24));
        log.file.write_all(b"second\n").unwrap();
        let read = |name: &str| std::fs::read_to_string(tmp.path().join(name)).unwrap();
        assert_eq!(read("log_20261023.log"), "first\n");
        assert_eq!(read("log_20261024.log"), "earlier\nsecond\n");
    }

    /// A write after midnight goes to the new day's file.
    #[test]
    fn a_write_on_a_later_day_rolls_to_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = DailyLogFile::open(tmp.path(), 10).unwrap();
        let yesterday = log.day.pred_opt().unwrap();
        log.day = yesterday;
        log.write_all(b"x\n").unwrap();
        assert!(log.day > yesterday);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(file_name(log.day))).unwrap(),
            "x\n"
        );
    }

    /// Opening and rolling both apply the retention with the writer's `keep`.
    #[test]
    fn opening_and_rolling_prune_to_keep() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["log_20261020.log", "log_20261021.log", "log.2026-10-22.log"] {
            std::fs::write(tmp.path().join(name), name).unwrap();
        }
        let mut log = DailyLogFile::open(tmp.path(), 2).unwrap();
        let today = file_name(log.day);
        assert_eq!(names(tmp.path()), ["log.2026-10-22.log", today.as_str()]);

        log.day = day(2026, 10, 22);
        log.roll_to(day(2026, 10, 23));
        let mut kept = vec![today, "log_20261023.log".to_owned()];
        kept.sort();
        assert_eq!(names(tmp.path()), kept);
    }

    /// A day whose file cannot be opened deletes nothing.
    #[test]
    fn a_failed_open_prunes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = DailyLogFile::open(tmp.path(), 1).unwrap();
        std::fs::write(tmp.path().join("log_20261020.log"), "old").unwrap();
        std::fs::create_dir(tmp.path().join("log_20261023.log")).unwrap();
        log.day = day(2026, 10, 22);
        log.roll_to(day(2026, 10, 23));
        assert!(tmp.path().join("log_20261020.log").exists());
    }

    /// A clock set back never moves the writes to an older day's file.
    #[test]
    fn an_earlier_day_keeps_the_current_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = DailyLogFile::open(tmp.path(), 10).unwrap();
        log.day = day(2026, 10, 24);
        log.roll_to(day(2026, 10, 23));
        assert_eq!(log.day, day(2026, 10, 24));
        assert!(!tmp.path().join("log_20261023.log").exists());
    }

    /// A day whose file cannot be opened keeps writing to the last one, and
    /// is not tried again before the next day.
    #[test]
    fn a_failed_roll_keeps_the_last_file_until_the_next_day() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = DailyLogFile::open(tmp.path(), 10).unwrap();
        let current = file_name(log.day);
        log.dir = tmp.path().join("gone");
        let next = log.day.succ_opt().unwrap();
        log.roll_to(next);
        assert_eq!(log.day, next, "not retried on the next write");
        log.write_all(b"kept\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(current)).unwrap(),
            "kept\n"
        );
    }
}

//! In-process log capture so the GUI and CLI can show recent records.
//!
//! `env_logger` writes to stderr, which a GUI-launched process does not have.
//! This layer keeps the last `CAPACITY` records in a ring buffer and exposes
//! them over IPC, and it lets the level be changed at runtime instead of only
//! through the environment at startup.

use std::{
    collections::VecDeque,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use log::{LevelFilter, Log, Metadata, Record};

/// How many records to keep. Enough to see what led up to a problem without
/// letting a chatty debug session grow without bound.
const CAPACITY: usize = 500;

static BUFFER: LazyLock<Mutex<VecDeque<String>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(CAPACITY)));
static STDERR_WRITABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static LEVEL: AtomicUsize = AtomicUsize::new(LevelFilter::Info as usize);

fn buffer() -> &'static Mutex<VecDeque<String>> {
    &BUFFER
}

fn level_from_index(index: usize) -> LevelFilter {
    match index {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

fn index_from_level(level: LevelFilter) -> usize {
    match level {
        LevelFilter::Off => 0,
        LevelFilter::Error => 1,
        LevelFilter::Warn => 2,
        LevelFilter::Info => 3,
        LevelFilter::Debug => 4,
        LevelFilter::Trace => 5,
    }
}

/// The level currently in effect.
pub fn current_level() -> LevelFilter {
    level_from_index(LEVEL.load(Ordering::Relaxed))
}

/// Change the level at runtime. Accepts the same names as `RUST_LOG`.
pub fn set_level(name: &str) -> Result<LevelFilter, String> {
    let level = match name.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        other => return Err(format!("unknown log level `{other}`")),
    };
    LEVEL.store(index_from_level(level), Ordering::Relaxed);
    log::set_max_level(level);
    // Announce outside any lock: `log::info!` re-enters this logger, and doing
    // that while holding the buffer lock would deadlock.
    log::info!("log level set to {level}");
    Ok(level)
}

/// The most recent `n` records, oldest first.
pub fn recent(n: usize) -> Vec<String> {
    let buffer = buffer().lock().unwrap_or_else(|e| e.into_inner());
    let start = buffer.len().saturating_sub(n);
    buffer.iter().skip(start).cloned().collect()
}

/// Monotonic counter of every record ever written.
///
/// The ring buffer drops old entries, so a byte offset would go stale; counting
/// from the start of the process lets a reader ask for "everything after N" and
/// get a contiguous tail even across evictions.
static TOTAL: AtomicUsize = AtomicUsize::new(0);

/// How many records have been written so far.
pub fn total() -> usize {
    TOTAL.load(Ordering::Relaxed)
}

/// Records written since `cursor`, plus the new cursor to pass next time.
///
/// Returns an empty list when nothing new arrived. If more than `CAPACITY`
/// records were produced since `cursor`, only the retained window comes back;
/// the new cursor still advances past the ones that were evicted, so the reader
/// never asks for them again.
pub fn since(cursor: usize) -> (Vec<String>, usize) {
    let total = TOTAL.load(Ordering::Relaxed);
    if cursor >= total {
        return (Vec::new(), total);
    }
    let buffer = buffer().lock().unwrap_or_else(|e| e.into_inner());
    let requested = total - cursor;
    let retained = buffer.len();
    // Everything requested that is still in the ring.
    let available = requested.min(retained);
    let start = retained - available;
    (buffer.iter().skip(start).cloned().collect(), total)
}

/// Installs the ring-buffer sink and applies `default_level`.
///
/// Must be called once, before any logging happens. Returns `false` when a
/// logger is already installed (a test harness, a second call), leaving that
/// logger in place rather than panicking.
pub fn init(default_level: LevelFilter) -> bool {
    LEVEL.store(index_from_level(default_level), Ordering::Relaxed);
    if log::set_logger(&CaptureLogger).is_err() {
        return false;
    }
    log::set_max_level(default_level);
    log::debug!(
        "logging initialised: requested {default_level}, effective {}, max {}",
        level_name(current_level()),
        log::max_level()
    );
    true
}

struct CaptureLogger;

impl Log for CaptureLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= current_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{} {} {}] {}",
            record.level(),
            record.target(),
            timestamp(),
            record.args()
        );
        {
            let mut buffer = buffer().lock().unwrap_or_else(|e| e.into_inner());
            if buffer.len() == CAPACITY {
                buffer.pop_front();
            }
            // Move the string in; it is only needed here and in the stderr
            // mirror below, which formats from the record again.
            buffer.push_back(line);
            TOTAL.fetch_add(1, Ordering::Relaxed);
        }

        // Mirror to stderr so journald still has the record. A GUI process
        // launched from Finder has no stderr; writing anyway is harmless but
        // pointless, so check once and skip it from then on.
        //
        // The line is rebuilt rather than cloned: `eprintln!` locks stderr and
        // this path runs for every record at debug level, so avoiding an
        // allocation per record is worth the extra format.
        if *STDERR_WRITABLE.get_or_init(stderr_is_open) {
            eprintln!(
                "[{} {} {}] {}",
                record.level(),
                record.target(),
                timestamp(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// Seconds since the Unix epoch, formatted as `HH:MM:SS`.
///
/// A full date is not worth pulling in a dependency for; log records are read
/// live, and the journal has precise timestamps when history matters.
/// Whether fd 2 exists and is not closed.
///
/// A tty or a pipe both qualify; only a closed descriptor fails
/// `fcntl(F_GETFL)`. Windows has no `fcntl`, and a GUI build there always has
/// a console handle, so the check is skipped rather than emulated.
#[cfg(unix)]
fn stderr_is_open() -> bool {
    unsafe { libc::fcntl(2, libc::F_GETFL) != -1 }
}

#[cfg(not(unix))]
fn stderr_is_open() -> bool {
    true
}

/// Seconds since the Unix epoch, read at most once per second.
///
/// Formatting a timestamp per record is measurable at debug level, and a log
/// view does not need sub-second resolution. The cached value is only used for
/// display; ordering comes from the ring buffer.
static CACHED_SECS: AtomicUsize = AtomicUsize::new(usize::MAX);

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as usize)
        .unwrap_or_default();

    // A relaxed load/store pair: worst case two threads compute the same
    // string, which is harmless.
    let mut secs = CACHED_SECS.load(Ordering::Relaxed);
    if secs == usize::MAX || secs != now {
        CACHED_SECS.store(now, Ordering::Relaxed);
        secs = now;
    }
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

/// Map a level name to the filter used for the stderr mirror.
pub fn level_name(level: LevelFilter) -> &'static str {
    match level {
        LevelFilter::Off => "off",
        LevelFilter::Error => "error",
        LevelFilter::Warn => "warn",
        LevelFilter::Info => "info",
        LevelFilter::Debug => "debug",
        LevelFilter::Trace => "trace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_round_trip() {
        for name in ["off", "error", "warn", "info", "debug", "trace"] {
            let level = set_level(name).expect("known level");
            assert_eq!(level_name(level), name);
        }
    }

    #[test]
    fn unknown_level_is_rejected() {
        assert!(set_level("verbose").is_err());
    }

    #[test]
    fn recent_returns_the_tail() {
        // The buffer is global and the logger may already be installed by
        // another test, so record through the sink directly rather than
        // assuming `log::info!` reaches it.
        let _ = set_level("info");
        let record = log::Record::builder()
            .args(format_args!("ring buffer probe"))
            .level(log::Level::Info)
            .target("logging::tests")
            .build();
        CaptureLogger.log(&record);
        let tail = recent(10);
        assert!(tail.iter().any(|l| l.contains("ring buffer probe")));
    }

    #[test]
    fn timestamp_is_second_resolution() {
        let a = timestamp();
        let b = timestamp();
        // Same second, so the cached value is reused and both render equal.
        assert_eq!(a, b);
        assert_eq!(a.len(), 8, "expected HH:MM:SS, got {a:?}");
        assert_eq!(a.as_bytes()[2], b':');
        assert_eq!(a.as_bytes()[5], b':');
    }

    #[test]
    fn level_filters_records() {
        let _ = set_level("warn");
        let info = log::Record::builder()
            .args(format_args!("should be filtered"))
            .level(log::Level::Info)
            .target("logging::tests")
            .build();
        CaptureLogger.log(&info);
        assert!(!recent(20).iter().any(|l| l.contains("should be filtered")));
        let _ = set_level("info");
    }
}

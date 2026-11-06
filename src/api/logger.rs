use crate::api::config::LOG_FIELD_MAX_BYTES;
use anyhow::Result;
use file_rotate::{compression::Compression, suffix::AppendCount, FileRotate};
use flutter_rust_bridge::frb;
use log::kv::{Error, Key, Value, VisitSource};
use log::LevelFilter;
use std::cell::Cell;
use std::fmt::Write;
use std::io::LineWriter;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError, RwLock};

const BYTES_PER_MB: u64 = 1_048_576;

/// Set once `init_logger` has installed the logger; later calls are no-ops.
static LOGGER_INSTALLED: Mutex<bool> = Mutex::new(false);

/// Levels the installed logger filters with; `None` until `init_logger` succeeds.
static LOG_LEVELS: RwLock<Option<LogLevels>> = RwLock::new(None);

/// Ceilings no config or `set_log_level` can raise a target above. `minotari::http`
/// logs full base-node URLs (including any `user:pass@`) at Debug.
const LEVEL_CEILINGS: &[(&str, LevelFilter)] = &[("minotari::http", LevelFilter::Info)];

thread_local! {
    /// True while this thread writes to the log file, i.e. holds fern's output lock.
    static IN_LOG_WRITE: Cell<bool> = const { Cell::new(false) };
}

/// Logging verbosity, from `Error` (least) to `Trace` (most), or `Off`.
#[frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Off,
}

impl From<LogLevel> for LevelFilter {
    fn from(l: LogLevel) -> Self {
        match l {
            LogLevel::Error => LevelFilter::Error,
            LogLevel::Warn => LevelFilter::Warn,
            LogLevel::Info => LevelFilter::Info,
            LogLevel::Debug => LevelFilter::Debug,
            LogLevel::Trace => LevelFilter::Trace,
            LogLevel::Off => LevelFilter::Off,
        }
    }
}

/// A per-module log-level override: log records whose `target` matches are
/// filtered at `level` instead of the default.
#[frb]
#[derive(Clone, Debug)]
pub struct LogModuleConfig {
    pub target: String,
    pub level: LogLevel,
}

/// Configuration for [`init_logger`].
///
/// `default_level` applies to all modules without a `module_levels` override;
/// `max_file_size_mb` and `max_files` bound the rotating on-disk log.
#[frb]
#[derive(Clone, Debug)]
pub struct LoggerConfig {
    pub default_level: LogLevel,
    pub module_levels: Vec<LogModuleConfig>,
    pub max_file_size_mb: u64,
    pub max_files: usize,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            default_level: LogLevel::Info,
            max_file_size_mb: 10,
            max_files: 5,
            module_levels: vec![
                LogModuleConfig {
                    target: "minotari".to_string(),
                    level: LogLevel::Info,
                },
                LogModuleConfig {
                    target: "minotari::http".to_string(),
                    level: LogLevel::Info,
                },
                LogModuleConfig {
                    target: "hyper".to_string(),
                    level: LogLevel::Warn,
                },
                LogModuleConfig {
                    target: "h2".to_string(),
                    level: LogLevel::Error,
                },
                LogModuleConfig {
                    target: "reqwest".to_string(),
                    level: LogLevel::Warn,
                },
            ],
        }
    }
}

/// The level filter behind the installed logger.
///
/// Until [`set_log_level`] is called the configured levels apply as-is. After it,
/// every target uses the new level, except overrides stricter than the configured
/// default (noise caps such as `hyper`), which still cap their target.
/// [`LEVEL_CEILINGS`] always apply.
struct LogLevels {
    configured_default: LevelFilter,
    modules: Vec<(String, LevelFilter)>,
    runtime_level: Option<LevelFilter>,
}

impl LogLevels {
    fn new(config: &LoggerConfig) -> Self {
        Self {
            configured_default: config.default_level.into(),
            modules: config
                .module_levels
                .iter()
                .map(|m| (m.target.clone(), m.level.into()))
                .collect(),
            runtime_level: None,
        }
    }

    fn level_for(&self, target: &str) -> LevelFilter {
        let mut module_level = None;
        let mut matched_len = 0;
        for (module, level) in &self.modules {
            if target_matches(target, module) && module.len() >= matched_len {
                module_level = Some(*level);
                matched_len = module.len();
            }
        }
        let level = match (module_level, self.runtime_level) {
            (None, None) => self.configured_default,
            (None, Some(runtime)) => runtime,
            (Some(module), None) => module,
            (Some(module), Some(runtime)) if module < self.configured_default => {
                module.min(runtime)
            }
            (Some(_), Some(runtime)) => runtime,
        };
        let mut level = level;
        for (module, ceiling) in LEVEL_CEILINGS {
            if target_matches(target, module) {
                level = level.min(*ceiling);
            }
        }
        level
    }

    /// The most verbose level any target can log at, for `log::set_max_level`.
    fn max_level(&self) -> LevelFilter {
        let mut max = self.runtime_level.unwrap_or(self.configured_default);
        for (module, _) in &self.modules {
            max = max.max(self.level_for(module));
        }
        max
    }
}

/// Whether `target` is `module` or a path below it (`module::...`).
fn target_matches(target: &str, module: &str) -> bool {
    target == module || (target.starts_with(module) && target[module.len()..].starts_with("::"))
}

/// Truncates `text` to [`LOG_FIELD_MAX_BYTES`] and escapes it for the log file.
fn sanitize_log_text(text: &str) -> String {
    if text.len() <= LOG_FIELD_MAX_BYTES {
        return escape_control(text);
    }
    let mut end = LOG_FIELD_MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = escape_control(&text[..end]);
    let _ = write!(out, "…[truncated {} bytes]", text.len() - end);
    out
}

/// Escapes backslashes, line breaks, control and bidi characters so logged text
/// cannot forge or visually reorder lines.
fn escape_control(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control()
                || matches!(
                    c,
                    '\u{2028}'
                        | '\u{2029}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                ) =>
            {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Marks the thread as inside a log write, so the panic hook does not log again
/// (fern's output lock is not reentrant). Never fails or panics: a failed write
/// drops the record, so logging cannot unwind the caller (e.g. mid-send) and fern
/// never falls back to printing the record to stderr.
struct LogWriteFlagged<W>(W);

/// Resets `IN_LOG_WRITE`, also when the write panics.
struct LogWriteGuard;

impl LogWriteGuard {
    fn enter() -> Self {
        IN_LOG_WRITE.set(true);
        LogWriteGuard
    }
}

impl Drop for LogWriteGuard {
    fn drop(&mut self) {
        IN_LOG_WRITE.set(false);
    }
}

impl<W: std::io::Write> std::io::Write for LogWriteFlagged<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _guard = LogWriteGuard::enter();
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.0.write(buf)));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _guard = LogWriteGuard::enter();
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| self.0.flush()));
        Ok(())
    }
}

fn record_enabled(metadata: &log::Metadata) -> bool {
    LOG_LEVELS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|l| metadata.level() <= l.level_for(metadata.target()))
}

struct KvTextVisitor<'a> {
    out: &'a mut String,
}

impl<'a, 'kvs> VisitSource<'kvs> for KvTextVisitor<'a> {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), Error> {
        write!(
            self.out,
            " {}={}",
            escape_control(key.as_str()),
            sanitize_log_text(&value.to_string())
        )
        .map_err(|_| Error::msg("fmt error"))
    }
}

/// Initialize the global logger, writing to a rotating file under `base_path/logs`
/// (and to stdout in debug builds only), and install a panic hook that logs Rust panics.
///
/// `base_path` is the directory to create the `logs/` subfolder in; `config`
/// (`None` → sensible defaults) tunes levels and rotation. Call once at startup;
/// calls after a successful one do nothing and return `Ok`.
/// Synchronous (`#[frb(sync)]`); errors if the logger cannot be installed.
#[frb(sync)]
pub fn init_logger(base_path: String, config: Option<LoggerConfig>) -> Result<()> {
    // The flag is only set after success, so a poisoned value is still accurate.
    let mut installed = LOGGER_INSTALLED
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if *installed {
        return Ok(());
    }
    let config = config.unwrap_or_default();
    // FileRotate::new panics on a zero size limit, so validate first.
    if config.max_file_size_mb == 0 {
        return Err(anyhow::anyhow!("max_file_size_mb must be greater than 0"));
    }
    let max_file_bytes =
        usize::try_from(config.max_file_size_mb.saturating_mul(BYTES_PER_MB)).unwrap_or(usize::MAX);

    let mut log_dir = PathBuf::from(base_path);
    log_dir.push("logs");
    std::fs::create_dir_all(&log_dir)?;
    // Best-effort: some filesystems or a foreign-owned `logs/` reject chmod.
    #[cfg(unix)]
    let chmod_error = {
        use std::os::unix::fs::PermissionsExt;
        let chmod_error =
            std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o700)).err();
        // Files from older versions may have been created with wider permissions.
        if let Ok(entries) = std::fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == "minotari.log" || name.starts_with("minotari.log.") {
                    let _ = std::fs::set_permissions(
                        entry.path(),
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
        }
        chmod_error
    };

    let log_file_path = log_dir.join("minotari.log");

    let file_rotator = FileRotate::new(
        log_file_path,
        AppendCount::new(config.max_files),
        file_rotate::ContentLimit::Bytes(max_file_bytes),
        Compression::None,
        #[cfg(unix)]
        Some(owner_only_open_options()),
    );
    let log_writer = LogWriteFlagged(LineWriter::new(file_rotator));

    let dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            let mut kv_output = String::new();
            let mut visitor = KvTextVisitor {
                out: &mut kv_output,
            };

            if let Err(e) = record.key_values().visit(&mut visitor) {
                let _ = write!(kv_output, " [KV Error: {}]", e);
            }

            out.finish(format_args!(
                "{}[{}][{}] {}{}",
                chrono::Local::now().format("[%Y-%m-%d %H:%M:%S]"),
                record.target(),
                record.level(),
                sanitize_log_text(&message.to_string()),
                kv_output
            ))
        })
        .filter(record_enabled)
        .chain(Box::new(log_writer) as Box<dyn std::io::Write + Send>);

    // On iOS and Android stdout ends up in the OS log, so release builds skip it.
    #[cfg(debug_assertions)]
    let dispatch = dispatch.chain(std::io::stdout());

    let levels = LogLevels::new(&config);
    let max_level = levels.max_level();
    set_levels(Some(levels));

    if let Err(e) = dispatch.apply() {
        set_levels(None);
        return Err(anyhow::anyhow!(
            "Logger could not be initialized: {}. Logs will go to the existing logger.",
            e
        ));
    }
    log::set_max_level(max_level);
    *installed = true;
    // std's default hook prints to stderr, which reaches the OS log, so release
    // builds record panics only in the log file.
    #[cfg(debug_assertions)]
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Logging from inside a log write would deadlock on fern's output lock.
        if !IN_LOG_WRITE.get() {
            log::error!(
                "RUST PANIC: {}\nBacktrace:\n{}",
                info,
                std::backtrace::Backtrace::force_capture()
            );
        }
        #[cfg(debug_assertions)]
        prev_hook(info);
    }));
    log::info!("Minotari Logger Initialized");
    #[cfg(unix)]
    if let Some(e) = chmod_error {
        log::warn!("could not restrict logs/ permissions: {e}");
    }
    Ok(())
}

/// file-rotate's default open options, plus owner-only (0o600) permissions.
#[cfg(unix)]
fn owner_only_open_options() -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).create(true).append(true).mode(0o600);
    options
}

fn set_levels(levels: Option<LogLevels>) {
    *LOG_LEVELS.write().unwrap_or_else(PoisonError::into_inner) = levels;
}

/// Change the log level at runtime, e.g. from an in-app debug toggle.
///
/// Every target logs at `level` from now on, except per-module overrides stricter
/// than the configured default (such as `hyper`), which still cap their target.
/// `minotari::http` always stays at most Info, because it logs base-node URLs.
/// Errors if [`init_logger`] has not installed the logger yet.
#[frb(sync)]
pub fn set_log_level(level: LogLevel) -> Result<()> {
    let max_level = {
        let mut guard = LOG_LEVELS.write().unwrap_or_else(PoisonError::into_inner);
        let Some(levels) = guard.as_mut() else {
            return Err(anyhow::anyhow!(
                "Logger is not initialized; call init_logger first"
            ));
        };
        levels.runtime_level = Some(level.into());
        levels.max_level()
    };
    log::set_max_level(max_level);
    log::info!("Log level set to {:?}", level);
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Enum-mapping tests are upstream-drift tripwires: if `log::LevelFilter`
    //! changes, they must be revisited. `init_logger_*` is the only test that
    //! installs the process-global logger.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn log_level_maps_to_level_filter_exhaustively() {
        assert_eq!(LevelFilter::from(LogLevel::Error), LevelFilter::Error);
        assert_eq!(LevelFilter::from(LogLevel::Warn), LevelFilter::Warn);
        assert_eq!(LevelFilter::from(LogLevel::Info), LevelFilter::Info);
        assert_eq!(LevelFilter::from(LogLevel::Debug), LevelFilter::Debug);
        assert_eq!(LevelFilter::from(LogLevel::Trace), LevelFilter::Trace);
        assert_eq!(LevelFilter::from(LogLevel::Off), LevelFilter::Off);
    }

    fn default_levels() -> LogLevels {
        LogLevels::new(&LoggerConfig::default())
    }

    #[test]
    fn default_config_keeps_levels_and_caps_reqwest() {
        let levels = default_levels();
        assert_eq!(
            levels.level_for("rust_lib_flutter_rust_wallet"),
            LevelFilter::Info
        );
        assert_eq!(levels.level_for("minotari::scan"), LevelFilter::Info);
        assert_eq!(levels.level_for("hyper"), LevelFilter::Warn);
        assert_eq!(levels.level_for("hyper::client"), LevelFilter::Warn);
        assert_eq!(levels.level_for("h2::codec"), LevelFilter::Error);
        assert_eq!(levels.level_for("reqwest::connect"), LevelFilter::Warn);
        assert_eq!(levels.max_level(), LevelFilter::Info);
    }

    #[test]
    fn module_override_matches_whole_path_segments_only() {
        let levels = default_levels();
        assert_eq!(levels.level_for("hyper_util::client"), LevelFilter::Info);
    }

    #[test]
    fn runtime_level_raises_everything_but_noise_caps() {
        let mut levels = default_levels();
        levels.runtime_level = Some(LevelFilter::Debug);
        assert_eq!(levels.level_for("minotari::scan"), LevelFilter::Debug);
        assert_eq!(levels.level_for("tari_common"), LevelFilter::Debug);
        assert_eq!(levels.level_for("hyper"), LevelFilter::Warn);
        assert_eq!(levels.level_for("h2"), LevelFilter::Error);
        assert_eq!(levels.max_level(), LevelFilter::Debug);
    }

    #[test]
    fn runtime_level_lowers_noise_caps_too() {
        let mut levels = default_levels();
        levels.runtime_level = Some(LevelFilter::Error);
        assert_eq!(levels.level_for("minotari"), LevelFilter::Error);
        assert_eq!(levels.level_for("hyper"), LevelFilter::Error);
        assert_eq!(levels.max_level(), LevelFilter::Error);
    }

    #[test]
    fn config_cannot_raise_minotari_http_above_info() {
        let config = LoggerConfig {
            default_level: LogLevel::Trace,
            module_levels: vec![LogModuleConfig {
                target: "minotari::http".to_string(),
                level: LogLevel::Trace,
            }],
            ..LoggerConfig::default()
        };
        let levels = LogLevels::new(&config);
        assert_eq!(levels.level_for("minotari::http::x"), LevelFilter::Info);
        assert_eq!(levels.level_for("minotari::scan"), LevelFilter::Trace);
    }

    #[test]
    fn toggle_never_raises_minotari_http_above_info() {
        let mut levels = default_levels();
        assert_eq!(levels.level_for("minotari::http::x"), LevelFilter::Info);
        levels.runtime_level = Some(LevelFilter::Trace);
        assert_eq!(levels.level_for("minotari::http::x"), LevelFilter::Info);
        assert_eq!(levels.level_for("minotari::http"), LevelFilter::Info);
        assert_eq!(levels.level_for("minotari::scan"), LevelFilter::Trace);
        assert_eq!(levels.level_for("minotari::httpx"), LevelFilter::Trace);
    }

    #[test]
    fn escape_control_keeps_each_record_on_one_line() {
        assert_eq!(escape_control("plain text é"), "plain text é");
        assert_eq!(
            escape_control("a\r\nb\tc\u{1b}[31m\u{0}"),
            "a\\r\\nb\\tc\\u{1b}[31m\\u{0}"
        );
        assert_eq!(escape_control("a\\nb"), "a\\\\nb");
        assert_eq!(
            escape_control("x\u{2028}\u{2029}\u{200e}\u{200f}\u{202a}\u{202e}\u{2066}\u{2069}y"),
            "x\\u{2028}\\u{2029}\\u{200e}\\u{200f}\\u{202a}\\u{202e}\\u{2066}\\u{2069}y"
        );
    }

    #[test]
    fn sanitize_log_text_keeps_short_text_and_truncates_long_text() {
        assert_eq!(sanitize_log_text("short\n"), "short\\n");

        let long = "a".repeat(LOG_FIELD_MAX_BYTES + 10);
        let out = sanitize_log_text(&long);
        assert_eq!(
            out,
            format!("{}…[truncated 10 bytes]", "a".repeat(LOG_FIELD_MAX_BYTES))
        );

        // A 2-byte char straddling the limit is cut before it, not split.
        let straddling = format!("{}é{}", "a".repeat(LOG_FIELD_MAX_BYTES - 1), "b".repeat(5));
        let out = sanitize_log_text(&straddling);
        assert_eq!(
            out,
            format!(
                "{}…[truncated 7 bytes]",
                "a".repeat(LOG_FIELD_MAX_BYTES - 1)
            )
        );
    }

    struct FlagRecorder(Vec<bool>);

    impl std::io::Write for FlagRecorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.push(IN_LOG_WRITE.get());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.push(IN_LOG_WRITE.get());
            Ok(())
        }
    }

    struct PanickingWriter;

    impl std::io::Write for PanickingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            panic!("write failed");
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn log_write_flag_is_set_only_during_writes() {
        use std::io::Write as _;
        let mut writer = LogWriteFlagged(FlagRecorder(Vec::new()));
        writer.write_all(b"line").unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.0 .0, vec![true, true]);
        assert!(!IN_LOG_WRITE.get());
    }

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
    }

    #[test]
    fn log_write_swallows_inner_panics_and_resets_the_flag() {
        use std::io::Write as _;
        let mut writer = LogWriteFlagged(PanickingWriter);
        assert_eq!(writer.write(b"line").unwrap(), 4);
        assert!(writer.write_all(b"again").is_ok());
        assert!(!IN_LOG_WRITE.get());
    }

    #[test]
    fn log_write_swallows_inner_errors() {
        use std::io::Write as _;
        let mut writer = LogWriteFlagged(FailingWriter);
        assert_eq!(writer.write(b"line").unwrap(), 4);
        assert!(writer.flush().is_ok());
        assert!(!IN_LOG_WRITE.get());
    }

    #[test]
    fn init_logger_rejects_bad_config_then_installs_once_and_enables_set_log_level() {
        assert!(set_log_level(LogLevel::Debug).is_err());

        let base = std::env::temp_dir().join(format!("cw_tari_logger_{}", std::process::id()));
        let base_path = base.to_string_lossy().to_string();
        // A rotated file left by an older version with wider permissions.
        let old_rotated = base.join("logs").join("minotari.log.1");
        std::fs::create_dir_all(base.join("logs")).unwrap();
        std::fs::write(&old_rotated, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&old_rotated, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let zero_size = LoggerConfig {
            max_file_size_mb: 0,
            ..LoggerConfig::default()
        };
        assert!(init_logger(base_path.clone(), Some(zero_size)).is_err());
        assert!(!LOGGER_INSTALLED.is_poisoned());
        assert!(set_log_level(LogLevel::Debug).is_err());

        init_logger(base_path.clone(), None).unwrap();
        init_logger(base_path, None).unwrap();

        log::info!(target: "logger_test", "info line");
        log::debug!(target: "logger_test", "debug line before toggle");
        set_log_level(LogLevel::Debug).unwrap();
        log::debug!(target: "logger_test", "debug line after toggle");

        log::info!(target: "logger_test", "forged\n[2026-01-01 00:00:00][x][INFO] line");

        let log_file = base.join("logs").join("minotari.log");
        let contents = std::fs::read_to_string(&log_file).unwrap();
        assert!(contents.contains("forged\\n[2026-01-01"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(base.join("logs"))
                .unwrap()
                .permissions()
                .mode();
            let file_mode = std::fs::metadata(&log_file).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700);
            assert_eq!(file_mode & 0o777, 0o600);
            let old_mode = std::fs::metadata(&old_rotated)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(old_mode & 0o777, 0o600);
        }
        assert!(contents.contains("Minotari Logger Initialized"));
        assert!(contents.contains("info line"));
        assert!(!contents.contains("debug line before toggle"));
        assert!(contents.contains("debug line after toggle"));
        let _ = std::fs::remove_dir_all(base);
    }
}

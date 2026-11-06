use anyhow::Result;
use file_rotate::{compression::Compression, suffix::AppendCount, FileRotate};
use flutter_rust_bridge::frb;
use log::kv::{Error, Key, Value, VisitSource};
use log::LevelFilter;
use std::fmt::Write;
use std::io::LineWriter;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

const BYTES_PER_MB: u64 = 1_048_576;

/// Set once `init_logger` has installed the logger; later calls are no-ops.
static LOGGER_INSTALLED: Mutex<bool> = Mutex::new(false);

/// Levels the installed logger filters with; `None` until `init_logger` succeeds.
static LOG_LEVELS: RwLock<Option<LogLevels>> = RwLock::new(None);

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
            let matches = target == module
                || (target.starts_with(module.as_str())
                    && target[module.len()..].starts_with("::"));
            if matches && module.len() >= matched_len {
                module_level = Some(*level);
                matched_len = module.len();
            }
        }
        match (module_level, self.runtime_level) {
            (None, None) => self.configured_default,
            (None, Some(runtime)) => runtime,
            (Some(module), None) => module,
            (Some(module), Some(runtime)) if module < self.configured_default => {
                module.min(runtime)
            }
            (Some(_), Some(runtime)) => runtime,
        }
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

fn record_enabled(metadata: &log::Metadata) -> bool {
    match LOG_LEVELS.read() {
        Ok(levels) => levels
            .as_ref()
            .is_some_and(|l| metadata.level() <= l.level_for(metadata.target())),
        Err(_) => false,
    }
}

struct KvTextVisitor<'a> {
    out: &'a mut String,
}

impl<'a, 'kvs> VisitSource<'kvs> for KvTextVisitor<'a> {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), Error> {
        write!(self.out, " {}={}", key, value).map_err(|_| Error::msg("fmt error"))
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
    let mut installed = LOGGER_INSTALLED
        .lock()
        .map_err(|_| anyhow::anyhow!("Logger state lock poisoned"))?;
    if *installed {
        return Ok(());
    }
    let config = config.unwrap_or_default();

    let mut log_dir = PathBuf::from(base_path);
    log_dir.push("logs");
    std::fs::create_dir_all(&log_dir)?;

    let log_file_path = log_dir.join("minotari.log");

    let file_rotator = FileRotate::new(
        log_file_path,
        AppendCount::new(config.max_files),
        file_rotate::ContentLimit::Bytes((config.max_file_size_mb * BYTES_PER_MB) as usize),
        Compression::None,
        #[cfg(unix)]
        None,
    );
    let log_writer = LineWriter::new(file_rotator);

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
                message,
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
    set_levels(Some(levels))?;

    if let Err(e) = dispatch.apply() {
        set_levels(None)?;
        return Err(anyhow::anyhow!(
            "Logger could not be initialized: {}. Logs will go to the existing logger.",
            e
        ));
    }
    log::set_max_level(max_level);
    *installed = true;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(
            "RUST PANIC: {}\nBacktrace:\n{}",
            info,
            std::backtrace::Backtrace::force_capture()
        );
        prev_hook(info);
    }));
    log::info!("Minotari Logger Initialized");
    Ok(())
}

fn set_levels(levels: Option<LogLevels>) -> Result<()> {
    let mut guard = LOG_LEVELS
        .write()
        .map_err(|_| anyhow::anyhow!("Logger state lock poisoned"))?;
    *guard = levels;
    Ok(())
}

/// Change the log level at runtime, e.g. from an in-app debug toggle.
///
/// Every target logs at `level` from now on, except per-module overrides stricter
/// than the configured default (such as `hyper`), which still cap their target.
/// Errors if [`init_logger`] has not installed the logger yet.
#[frb(sync)]
pub fn set_log_level(level: LogLevel) -> Result<()> {
    let max_level = {
        let mut guard = LOG_LEVELS
            .write()
            .map_err(|_| anyhow::anyhow!("Logger state lock poisoned"))?;
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
    fn init_logger_writes_file_is_idempotent_and_enables_set_log_level() {
        assert!(set_log_level(LogLevel::Debug).is_err());

        let base = std::env::temp_dir().join(format!("cw_tari_logger_{}", std::process::id()));
        let base_path = base.to_string_lossy().to_string();
        init_logger(base_path.clone(), None).unwrap();
        init_logger(base_path, None).unwrap();

        log::info!(target: "logger_test", "info line");
        log::debug!(target: "logger_test", "debug line before toggle");
        set_log_level(LogLevel::Debug).unwrap();
        log::debug!(target: "logger_test", "debug line after toggle");

        let contents = std::fs::read_to_string(base.join("logs").join("minotari.log")).unwrap();
        assert!(contents.contains("Minotari Logger Initialized"));
        assert!(contents.contains("info line"));
        assert!(!contents.contains("debug line before toggle"));
        assert!(contents.contains("debug line after toggle"));
        let _ = std::fs::remove_dir_all(base);
    }
}

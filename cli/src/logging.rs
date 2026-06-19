use color_eyre::eyre::{Context, Result};
use rolling_file::{BasicRollingFileAppender, RollingConditionBasic};
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{LogFormat, LogLevel, LoggingConfig};

#[derive(Debug)]
pub struct LogGuards {
    _stdout_guard: WorkerGuard,
    _file_guard: Option<WorkerGuard>,
}

/// Initialize logging.
///
/// Returns a drop guard responsible for flushing any remaining logs when the program terminates.
/// The guard must be assigned to a binding that is not _, as _ will result in the guard being dropped immediately.
pub fn init(config: &LoggingConfig) -> Result<LogGuards> {
    let filter = build_tracing_filter(config.log_level);
    let (stdout, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());
    let file = config
        .file_path
        .as_ref()
        .map(|_| rolling_file_appender(config))
        .transpose()
        .wrap_err("failed to configure Emerald log file")?;
    let file = file.map(tracing_appender::non_blocking);

    match (config.log_format, file) {
        (LogFormat::Plaintext, Some((file_writer, file_guard))) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .compact()
                        .with_target(false)
                        .with_ansi(enable_ansi())
                        .with_timer(UtcTime::rfc_3339())
                        .with_thread_ids(false)
                        .with_writer(stdout),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .compact()
                        .with_target(false)
                        .with_ansi(false)
                        .with_timer(UtcTime::rfc_3339())
                        .with_thread_ids(false)
                        .with_writer(file_writer),
                )
                .init();
            Ok(LogGuards {
                _stdout_guard: stdout_guard,
                _file_guard: Some(file_guard),
            })
        }
        (LogFormat::Plaintext, None) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .compact()
                        .with_target(false)
                        .with_ansi(enable_ansi())
                        .with_timer(UtcTime::rfc_3339())
                        .with_thread_ids(false)
                        .with_writer(stdout),
                )
                .init();
            Ok(LogGuards {
                _stdout_guard: stdout_guard,
                _file_guard: None,
            })
        }
        (LogFormat::Json, Some((file_writer, file_guard))) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_target(false)
                        .with_ansi(false)
                        .with_timer(UtcTime::rfc_3339())
                        .with_thread_ids(false)
                        .with_writer(stdout),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_target(false)
                        .with_ansi(false)
                        .with_timer(UtcTime::rfc_3339())
                        .with_thread_ids(false)
                        .with_writer(file_writer),
                )
                .init();
            Ok(LogGuards {
                _stdout_guard: stdout_guard,
                _file_guard: Some(file_guard),
            })
        }
        (LogFormat::Json, None) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_target(false)
                        .with_ansi(false)
                        .with_timer(UtcTime::rfc_3339())
                        .with_thread_ids(false)
                        .with_writer(stdout),
                )
                .init();
            Ok(LogGuards {
                _stdout_guard: stdout_guard,
                _file_guard: None,
            })
        }
    }
}

pub(crate) fn rolling_file_appender(
    config: &LoggingConfig,
) -> std::io::Result<BasicRollingFileAppender> {
    let path = config.file_path.as_deref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "log file path is required for file logging",
        )
    })?;

    BasicRollingFileAppender::new(
        path,
        RollingConditionBasic::new().max_size(config.file_max_size_bytes),
        config.file_max_files,
    )
}

/// Check if both stdout and stderr are proper terminal (tty),
/// so that we know whether or not to enable colored output,
/// using ANSI escape codes. If either is not, eg. because
/// stdout is redirected to a file, we don't enable colored output.
pub fn enable_ansi() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
}

/// Common prefixes of the crates targeted by the default log level.
const TARGET_CRATES: &[&str] = &[
    "informalsystems_malachitebft",
    "malachitebft_eth",
    "emerald",
    "key_provider",
];

/// Build a tracing directive setting the log level for the
/// crates to the given `log_level`.
pub fn default_directive(log_level: LogLevel) -> String {
    use itertools::Itertools;

    TARGET_CRATES
        .iter()
        .map(|&c| format!("{c}={log_level}"))
        .join(",")
}

/// Builds a tracing filter based on the input `log_level`.
/// Returns error if the filter failed to build.
fn build_tracing_filter(log_level: LogLevel) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .parse(default_directive(log_level))
            .unwrap()
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::rolling_file_appender;
    use crate::config::LoggingConfig;

    #[test]
    fn rolling_file_appender_creates_active_file() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("emerald-logging-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let log_path = dir.join("emerald.log");
        let config = LoggingConfig {
            file_path: Some(log_path.clone()),
            file_max_size_bytes: 64,
            file_max_files: 2,
            ..LoggingConfig::default()
        };

        let mut appender = rolling_file_appender(&config)?;
        writeln!(appender, "json log line")?;
        appender.flush()?;

        assert!(log_path.exists());
        let _ = std::fs::remove_file(log_path);
        let _ = std::fs::remove_dir(dir);
        Ok(())
    }
}

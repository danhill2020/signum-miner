use crate::config::Cfg;

use log::LevelFilter;
use log4rs::append::console::ConsoleAppender;
use log4rs::append::rolling_file::policy::compound::roll::fixed_window::FixedWindowRoller;
use log4rs::append::rolling_file::policy::compound::trigger::size::SizeTrigger;
use log4rs::append::rolling_file::policy::compound::CompoundPolicy;
use log4rs::append::rolling_file::RollingFileAppender;
use log4rs::config::{Appender, Config, Root};
use log4rs::encode::pattern::PatternEncoder;
use log4rs::filter::threshold::ThresholdFilter;
use std::io::IsTerminal;

fn to_log_level(s: &str, default: log::LevelFilter) -> log::LevelFilter {
    match s.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        "off" => log::LevelFilter::Off,
        _ => default,
    }
}

/// The progress bar only makes sense on an interactive terminal.  Under
/// systemd, in a container, or with output redirected to a file it would just
/// flood the log with carriage returns, so it is switched off there.
pub fn effective_show_progress(cfg: &Cfg) -> bool {
    cfg.show_progress && std::io::stdout().is_terminal()
}

pub fn init_logger(cfg: &Cfg) -> Result<log4rs::Handle, String> {
    let level_console = to_log_level(&cfg.console_log_level, log::LevelFilter::Info);
    let level_logfile = to_log_level(&cfg.logfile_log_level, log::LevelFilter::Warn);

    // The carriage return lets log lines overwrite the progress bar on a
    // terminal; it has no business in a log file.
    let mut console_log_pattern = if effective_show_progress(cfg) {
        "\r".to_owned()
    } else {
        "".to_owned()
    };
    console_log_pattern.push_str(&cfg.console_log_pattern);

    let stdout = ConsoleAppender::builder()
        .encoder(Box::new(PatternEncoder::new(&console_log_pattern)))
        .build();

    let config = if level_logfile == log::LevelFilter::Off {
        Config::builder()
            .appender(
                Appender::builder()
                    .filter(Box::new(ThresholdFilter::new(level_console)))
                    .build("stdout", Box::new(stdout)),
            )
            .build(Root::builder().appender("stdout").build(LevelFilter::Trace))
    } else {
        let roller = FixedWindowRoller::builder()
            .base(1)
            .build("log/signum-miner.{}.log", cfg.logfile_max_count)
            .map_err(|e| format!("invalid log roller configuration: {}", e))?;
        let trigger = SizeTrigger::new(cfg.logfile_max_size * 1024 * 1024);
        let policy = Box::new(CompoundPolicy::new(Box::new(trigger), Box::new(roller)));
        let logfile = RollingFileAppender::builder()
            .encoder(Box::new(PatternEncoder::new(&cfg.logfile_log_pattern)))
            .build("log/signum-miner.1.log", policy)
            .map_err(|e| {
                format!(
                    "cannot open log file log/signum-miner.1.log: {} (set logfile_log_level: off to disable file logging)",
                    e
                )
            })?;
        Config::builder()
            .appender(
                Appender::builder()
                    .filter(Box::new(ThresholdFilter::new(level_console)))
                    .build("stdout", Box::new(stdout)),
            )
            .appender(
                Appender::builder()
                    .filter(Box::new(ThresholdFilter::new(level_logfile)))
                    .build("logfile", Box::new(logfile)),
            )
            .build(
                Root::builder()
                    .appender("stdout")
                    .appender("logfile")
                    .build(LevelFilter::Trace),
            )
    }
    .map_err(|e| format!("invalid logger configuration: {}", e))?;

    log4rs::init_config(config).map_err(|e| format!("failed to initialise logging: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_log_level() {
        assert_eq!(
            to_log_level("Trace", log::LevelFilter::Error),
            log::LevelFilter::Trace
        );
        assert_eq!(
            to_log_level("Foo", log::LevelFilter::Error),
            log::LevelFilter::Error
        );
        assert_eq!(
            to_log_level("DEBUG", log::LevelFilter::Error),
            log::LevelFilter::Debug
        );
        assert_eq!(
            to_log_level("InFo", log::LevelFilter::Error),
            log::LevelFilter::Info
        );
        assert_eq!(
            to_log_level("eRROR", log::LevelFilter::Info),
            log::LevelFilter::Error
        );
        assert_eq!(
            to_log_level("WARN", log::LevelFilter::Info),
            log::LevelFilter::Warn
        );
        assert_eq!(
            to_log_level("Off", log::LevelFilter::Info),
            log::LevelFilter::Off
        );
    }

    #[test]
    fn test_init_logger() {
        use crate::config::parse_cfg;
        // File logging off so the test does not create a log/ directory.
        let mut cfg = parse_cfg(
            "plot_dirs: []\nurl: 'http://localhost'\nlogfile_log_level: off\n",
            "inline",
        )
        .expect("Failed to load config");
        cfg.console_log_level = log::LevelFilter::Error.to_string();

        // Only one logger can be installed per process; a second init (e.g.
        // when tests run in one binary) must fail cleanly rather than panic.
        let _ = init_logger(&cfg);

        trace!("TRACE");
        debug!("DEBUG");
        info!("INFO");
    }
}

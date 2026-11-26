//! Diagnostics: `tracing` to standard error, or to daily log files when the
//! program runs as a Windows service, which has no standard error.

use tracing_subscriber::EnvFilter;

/// Install the subscriber. `RUST_LOG` wins; otherwise `default` (a level
/// such as `warn`) is raised one step per `-v`.
pub fn init(verbose: u8, default: &str) {
    let levels = ["error", "warn", "info", "debug", "trace"];
    let base = levels
        .iter()
        .position(|level| *level == default)
        .unwrap_or(1);
    let level = levels[(base + verbose as usize).min(levels.len() - 1)];
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    #[cfg(windows)]
    if bella_service::running_as_service()
        && let Some(dir) = bella_service::service_log_dir()
    {
        let files = daily::DailyFiles::new(dir);
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(move || files.writer())
            .try_init();
        return;
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(windows)]
mod daily {
    //! One log file per UTC day: `bella-YYYY-MM-DD.log` in the service's
    //! log directory, appended to.

    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, PoisonError};

    #[derive(Clone)]
    pub struct DailyFiles {
        inner: Arc<Mutex<State>>,
    }

    struct State {
        dir: PathBuf,
        day: i64,
        file: Option<File>,
    }

    pub struct Writer(Arc<Mutex<State>>);

    impl DailyFiles {
        pub fn new(dir: PathBuf) -> Self {
            let _ = std::fs::create_dir_all(&dir);
            Self {
                inner: Arc::new(Mutex::new(State {
                    dir,
                    day: -1,
                    file: None,
                })),
            }
        }

        pub fn writer(&self) -> Writer {
            Writer(self.inner.clone())
        }
    }

    fn today() -> i64 {
        (bella_core::UnixMillis::now().0 / 86_400_000) as i64
    }

    impl Write for Writer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let day = today();
            if state.day != day || state.file.is_none() {
                let (year, month, date) = crate::output::civil_from_days(day);
                let path = state
                    .dir
                    .join(format!("bella-{year:04}-{month:02}-{date:02}.log"));
                state.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
                state.day = day;
            }
            match state.file.as_mut() {
                Some(file) => file.write(buf),
                None => Ok(buf.len()),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            match state.file.as_mut() {
                Some(file) => file.flush(),
                None => Ok(()),
            }
        }
    }
}

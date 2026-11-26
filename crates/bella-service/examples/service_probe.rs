//! The program the live service tests install. It records how it was started
//! in the file named by its first argument and runs until it is stopped.
//!
//! Under the Windows Service Control Manager it also returns an error when a
//! file called `<record>.fail` appears, and notes a requested stop.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(100);

fn record(path: &Path) -> std::io::Result<()> {
    let mut text = format!(
        "pid={}\ncwd={}\nlog_dir={}\nvalue={}\n",
        std::process::id(),
        std::env::current_dir()?.display(),
        bella_service::service_log_dir()
            .unwrap_or_default()
            .display(),
        std::env::var("PROBE_VALUE").unwrap_or_default(),
    );
    for argument in std::env::args().skip(2) {
        text.push_str(&format!("arg={argument}\n"));
    }
    // Renamed into place so a reader never sees half a record.
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, text)?;
    fs::rename(temporary, path)
}

fn main() -> Result<(), Box<dyn Error>> {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: service_probe <record file> [arguments…]")?,
    );
    #[cfg(windows)]
    if bella_service::running_as_service() {
        let name = std::env::var(bella_service::ENV_SERVICE_NAME)?;
        bella_service::run_as_service(&name, move |stop| -> std::io::Result<()> {
            record(&path)?;
            let fail = path.with_extension("fail");
            while !stop.wait_timeout(POLL) {
                if fail.exists() {
                    fs::remove_file(&fail)?;
                    return Err(std::io::Error::other("asked to fail"));
                }
            }
            fs::write(path.with_extension("stopped"), "stopped")
        })?;
        return Ok(());
    }
    record(&path)?;
    loop {
        thread::sleep(POLL);
    }
}

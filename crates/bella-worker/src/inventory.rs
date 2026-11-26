//! The machine's inventory as the worker reports it, and the cached
//! containment commissioning result.

use bella_contain::Containment;
use bella_core::machine::{ExecutorInfo, Inventory, OsInfo};
use bella_core::platform::IsolationLevel;
use bella_core::resources::EnforcementReport;
use serde::{Deserialize, Serialize};
use std::path::Path;

const COMMISSIONING_FILE: &str = "commissioning.json";
const INVENTORY_FILE: &str = "inventory.json";

/// The verified enforcement report of one adapter on one OS build.
#[derive(Serialize, Deserialize)]
struct Commissioning {
    mechanism: String,
    os_version: String,
    kernel: String,
    report: EnforcementReport,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    let written = serde_json::to_vec_pretty(value)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            let partial = path.with_extension("json.tmp");
            std::fs::write(&partial, bytes)?;
            std::fs::rename(&partial, path)
        });
    if let Err(error) = written {
        tracing::warn!(path = %path.display(), %error, "could not store worker state");
    }
}

/// The native executor of this machine with what its adapter enforces.
///
/// The adapter's commissioning self-test runs the first time, and its
/// verified report is cached in `data_dir`. Later calls reuse the cached
/// report as long as the adapter's mechanism and the OS version and kernel
/// are unchanged, and run the self-test again otherwise. When the self-test
/// itself fails, the adapter's unverified `enforcement()` is returned with a
/// note saying why, and nothing is cached.
pub fn executor_info(data_dir: &Path, containment: &dyn Containment, os: &OsInfo) -> ExecutorInfo {
    let mechanism = containment.mechanism();
    let path = data_dir.join(COMMISSIONING_FILE);
    let cached = read_json::<Commissioning>(&path).filter(|cached| {
        cached.mechanism == mechanism
            && cached.os_version == os.os_version
            && cached.kernel == os.kernel
            && cached.report.verified
    });
    let enforcement = match cached {
        Some(cached) => cached.report,
        None => match containment.self_test() {
            Ok(report) => {
                write_json(
                    &path,
                    &Commissioning {
                        mechanism: mechanism.to_string(),
                        os_version: os.os_version.clone(),
                        kernel: os.kernel.clone(),
                        report: report.clone(),
                    },
                );
                report
            }
            Err(error) => {
                tracing::warn!(%error, "containment self-test failed");
                let mut report = containment.enforcement();
                report.verified = false;
                report
                    .notes
                    .push(format!("commissioning self-test failed: {error}"));
                report
            }
        },
    };
    ExecutorInfo {
        isolation: IsolationLevel::Native,
        mechanism: mechanism.to_string(),
        enforcement,
    }
}

/// Probe the machine and number the result against the inventory stored by
/// the previous start, so the revision grows only when something changed.
pub(crate) fn build(
    data_dir: &Path,
    containment: &dyn Containment,
    worker_version: &str,
) -> Inventory {
    let os = bella_probe::os_info();
    let executor = executor_info(data_dir, containment, &os);
    let path = data_dir.join(INVENTORY_FILE);
    let previous: Option<Inventory> = read_json(&path);
    let inventory = bella_probe::inventory(worker_version, vec![executor], previous.as_ref());
    if previous.as_ref() != Some(&inventory) {
        write_json(&path, &inventory);
    }
    inventory
}

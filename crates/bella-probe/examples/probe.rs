//! Print this machine's inventory, boot id and three telemetry samples as
//! JSON.
//!
//! ```text
//! cargo run -p bella-probe --example probe
//! ```

use bella_probe::{ManagedUsage, ProbeOptions, Sampler};
use std::time::{Duration, Instant};

fn main() {
    let options = ProbeOptions::default();
    let started = Instant::now();
    let inventory =
        bella_probe::inventory_with(&options, env!("CARGO_PKG_VERSION"), Vec::new(), None);
    let inventory_ms = started.elapsed().as_millis();

    let work_dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    let mut sampler = Sampler::new(&work_dir);
    let mut samples = Vec::new();
    for _ in 0..3 {
        std::thread::sleep(Duration::from_secs(1));
        let telemetry = sampler.sample(ManagedUsage::default());
        samples.push(serde_json::json!({
            "telemetry": telemetry,
            "counters": format!("{:?}", sampler.last_counters()),
        }));
    }

    let report = serde_json::json!({
        "boot_id": bella_probe::boot_id(),
        "inventory_ms": inventory_ms,
        "inventory": inventory,
        "samples": samples,
    });
    match serde_json::to_string_pretty(&report) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("could not serialise the report: {error}"),
    }
}

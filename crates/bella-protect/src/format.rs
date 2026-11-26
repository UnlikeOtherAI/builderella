//! Number formatting for reasons and rejection messages.

use bella_core::defaults::GIB;
use std::time::Duration;

/// Bytes as GiB with one decimal and no unit, e.g. `14.2`.
pub(crate) fn gib_bare(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / GIB as f64)
}

/// Bytes as GiB with one decimal, e.g. `14.2 GiB`.
pub(crate) fn gib(bytes: u64) -> String {
    format!("{} GiB", gib_bare(bytes))
}

/// Milli-CPUs as CPUs with one decimal and no unit, e.g. `7.5`.
pub(crate) fn cpus_bare(milli: u64) -> String {
    format!("{:.1}", milli as f64 / 1000.0)
}

/// Milli-CPUs as CPUs with one decimal, e.g. `7.5 CPUs`.
pub(crate) fn cpus(milli: u64) -> String {
    format!("{} CPUs", cpus_bare(milli))
}

/// A duration in seconds with one decimal, e.g. `5.0 s`.
pub(crate) fn secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::defaults::MIB;

    #[test]
    fn one_decimal_everywhere() {
        assert_eq!(gib(18 * GIB), "18.0 GiB");
        assert_eq!(gib(512 * MIB), "0.5 GiB");
        assert_eq!(gib_bare(10 * GIB + 205 * MIB), "10.2");
        assert_eq!(cpus(7500), "7.5 CPUs");
        assert_eq!(cpus_bare(8000), "8.0");
        assert_eq!(secs(Duration::from_millis(7200)), "7.2 s");
    }
}

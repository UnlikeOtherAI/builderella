//! Human-readable quantities for explanations. Integer arithmetic only, so
//! the same input always prints the same text.

use bella_core::defaults::{GIB, MIB};
use bella_core::resources::ResourceVector;

const KIB: u64 = 1024;

fn scaled(n: u64, unit: u64, label: &str) -> String {
    let tenths = (u128::from(n) * 10 + u128::from(unit) / 2) / u128::from(unit);
    if tenths % 10 == 0 {
        format!("{} {label}", tenths / 10)
    } else {
        format!("{}.{} {label}", tenths / 10, tenths % 10)
    }
}

/// `18 GiB`, `1.5 GiB`, `512 MiB`.
pub(crate) fn bytes(n: u64) -> String {
    if n == u64::MAX {
        "unlimited".to_string()
    } else if n >= GIB {
        scaled(n, GIB, "GiB")
    } else if n >= MIB {
        scaled(n, MIB, "MiB")
    } else if n >= KIB {
        scaled(n, KIB, "KiB")
    } else {
        format!("{n} B")
    }
}

/// `4 CPUs`, `2.5 CPUs`, `1 CPU`.
pub(crate) fn cpus(milli: u64) -> String {
    let whole = milli / 1000;
    let frac = milli % 1000;
    if frac == 0 {
        let noun = if whole == 1 { "CPU" } else { "CPUs" };
        return format!("{whole} {noun}");
    }
    let digits = format!("{frac:03}");
    format!("{whole}.{} CPUs", digits.trim_end_matches('0'))
}

/// `45 s`, `14 min`, `2 h 5 min`.
pub(crate) fn duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs} s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} min");
    }
    let (hours, rest) = (mins / 60, mins % 60);
    if rest == 0 {
        format!("{hours} h")
    } else {
        format!("{hours} h {rest} min")
    }
}

/// The quantity alone, in the unit of its dimension: `18 GiB`, `12 CPUs`, `2`.
/// `dimension` is a `Shortfall::dimension`.
pub(crate) fn bare(dimension: &str, value: u64) -> String {
    match dimension {
        "cpu" => cpus(value),
        "memory" | "disk" => bytes(value),
        _ => value.to_string(),
    }
}

/// The quantity with what it is: `18 GiB memory`, `12 CPUs`,
/// `200 GiB scratch disk`, `2 \`gpu\` tokens`.
pub(crate) fn amount(dimension: &str, value: u64) -> String {
    match dimension {
        "cpu" => cpus(value),
        "memory" => format!("{} memory", bytes(value)),
        "disk" => format!("{} scratch disk", bytes(value)),
        other => {
            let name = other.strip_prefix("token:").unwrap_or(other);
            let noun = if value == 1 { "token" } else { "tokens" };
            format!("{value} `{name}` {noun}")
        }
    }
}

/// One dimension of a vector, addressed by `Shortfall::dimension`.
pub(crate) fn dimension_of(v: &ResourceVector, dimension: &str) -> u64 {
    match dimension {
        "cpu" => v.cpu_milli,
        "memory" => v.memory_bytes,
        "disk" => v.disk_bytes,
        other => v.token(other.strip_prefix("token:").unwrap_or(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_the_largest_unit_that_fits() {
        assert_eq!(bytes(18 * GIB), "18 GiB");
        assert_eq!(bytes(GIB + GIB / 2), "1.5 GiB");
        assert_eq!(bytes(512 * MIB), "512 MiB");
        assert_eq!(bytes(2048), "2 KiB");
        assert_eq!(bytes(17), "17 B");
        assert_eq!(bytes(u64::MAX), "unlimited");
    }

    #[test]
    fn cpus_keep_fractions_without_padding() {
        assert_eq!(cpus(4000), "4 CPUs");
        assert_eq!(cpus(1000), "1 CPU");
        assert_eq!(cpus(2500), "2.5 CPUs");
        assert_eq!(cpus(250), "0.25 CPUs");
    }

    #[test]
    fn durations_round_down_to_readable_units() {
        assert_eq!(duration_ms(45_999), "45 s");
        assert_eq!(duration_ms(14 * 60_000), "14 min");
        assert_eq!(duration_ms(125 * 60_000), "2 h 5 min");
        assert_eq!(duration_ms(120 * 60_000), "2 h");
    }

    #[test]
    fn amounts_name_their_dimension() {
        assert_eq!(amount("memory", 18 * GIB), "18 GiB memory");
        assert_eq!(amount("cpu", 12_000), "12 CPUs");
        assert_eq!(amount("disk", 200 * GIB), "200 GiB scratch disk");
        assert_eq!(amount("token:gpu", 2), "2 `gpu` tokens");
        assert_eq!(amount("token:gui-session", 1), "1 `gui-session` token");
        assert_eq!(bare("token:gpu", 2), "2");
        let v = ResourceVector::new(1000, 2, 3).with_token("gpu", 4);
        assert_eq!(dimension_of(&v, "cpu"), 1000);
        assert_eq!(dimension_of(&v, "memory"), 2);
        assert_eq!(dimension_of(&v, "disk"), 3);
        assert_eq!(dimension_of(&v, "token:gpu"), 4);
    }
}

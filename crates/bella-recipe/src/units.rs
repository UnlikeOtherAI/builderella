//! Human units: sizes (`8GiB`), durations (`1h30m`) and CPU counts (`0.5`).
//!
//! Every error message names the text that was rejected, so it can be shown
//! next to a file position without more context.

/// Size units and their byte multipliers. Decimal units are powers of 1000,
/// binary units powers of 1024.
const BYTE_UNITS: [(&str, u64); 9] = [
    ("B", 1),
    ("KB", 1000),
    ("MB", 1000 * 1000),
    ("GB", 1000 * 1000 * 1000),
    ("TB", 1000 * 1000 * 1000 * 1000),
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("TiB", 1 << 40),
];

/// Duration units in the only order they may appear, with milliseconds each.
const DURATION_UNITS: [(&str, u64); 4] = [("h", 3_600_000), ("m", 60_000), ("s", 1000), ("ms", 1)];

/// More CPUs than any machine has; rejects typos such as a memory size in
/// the `cpu` field.
const MAX_CPUS: f64 = 1_000_000.0;

/// Parse a size such as `512MiB`, `8GiB`, `1.5GB` or `4096B` into bytes.
///
/// Units are `B`, `KB`, `MB`, `GB`, `TB` (powers of 1000) and `KiB`, `MiB`,
/// `GiB`, `TiB` (powers of 1024), matched without regard to case. A unit is
/// required, and the result must be a whole number of bytes.
pub fn parse_bytes(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, unit) = (&trimmed[..split], trimmed[split..].trim());
    if number.is_empty() {
        return Err(format!(
            "`{text}` is not a size: expected a number followed by a unit, such as `8GiB`"
        ));
    }
    if unit.is_empty() {
        return Err(format!(
            "`{text}` has no unit: write `{number}B` for bytes, or use {}",
            unit_list(&BYTE_UNITS[1..])
        ));
    }
    let Some((_, multiplier)) = BYTE_UNITS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(unit))
    else {
        return Err(format!(
            "`{text}` has unknown size unit `{unit}`: use {}",
            unit_list(&BYTE_UNITS)
        ));
    };
    match scale_decimal(number, *multiplier) {
        Ok(bytes) => Ok(bytes),
        Err(DecimalError::Malformed) => Err(format!(
            "`{text}` is not a size: `{number}` is not a number"
        )),
        Err(DecimalError::Fractional) => Err(format!("`{text}` is not a whole number of bytes")),
        Err(DecimalError::Overflow) => Err(format!("`{text}` is too large")),
    }
}

/// Parse a duration such as `500ms`, `45s`, `30m` or `1h30m` into
/// milliseconds.
///
/// Units are `h`, `m`, `s` and `ms`, matched without regard to case. Parts
/// are whole numbers, go from the largest unit to the smallest, and each
/// unit appears at most once. Spaces between parts are allowed.
pub fn parse_duration_ms(text: &str) -> Result<u64, String> {
    let malformed = || {
        format!(
            "`{text}` is not a duration: expected whole numbers with units, such as `90s` or `1h30m`"
        )
    };
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(malformed());
    }
    let mut total: u64 = 0;
    // Index into DURATION_UNITS that the next part must reach or pass.
    let mut next_unit = 0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return Err(malformed());
        }
        let (number, after) = rest.split_at(digits);
        let after = after.trim_start();
        let letters = after
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(after.len());
        let (unit, after) = after.split_at(letters);
        if unit.is_empty() {
            let place = if number == text.trim() {
                String::new()
            } else {
                format!(" after `{number}`")
            };
            return Err(format!(
                "`{text}` has no unit{place}: use {}, as in `{number}m`",
                unit_list(&DURATION_UNITS)
            ));
        }
        let Some(position) = DURATION_UNITS
            .iter()
            .position(|(name, _)| name.eq_ignore_ascii_case(unit))
        else {
            return Err(format!(
                "`{text}` has unknown duration unit `{unit}`: use {}",
                unit_list(&DURATION_UNITS)
            ));
        };
        if position < next_unit {
            return Err(format!(
                "`{text}` is not a duration: write each unit once, largest first, such as `1h30m`"
            ));
        }
        next_unit = position + 1;
        let too_large = || format!("`{text}` is too long");
        let value: u64 = number.parse().map_err(|_| too_large())?;
        total = value
            .checked_mul(DURATION_UNITS[position].1)
            .and_then(|part| total.checked_add(part))
            .ok_or_else(too_large)?;
        rest = after.trim_start();
    }
    Ok(total)
}

/// Parse a duration that is stored in whole seconds.
pub fn parse_duration_secs(text: &str) -> Result<u64, String> {
    let ms = parse_duration_ms(text)?;
    if !ms.is_multiple_of(1000) {
        return Err(format!("`{text}` is not a whole number of seconds"));
    }
    Ok(ms / 1000)
}

/// Parse a CPU count such as `4` or `0.5` into thousandths of one logical
/// CPU.
pub fn parse_cpu(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let plain = !trimmed.is_empty()
        && trimmed.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && trimmed.bytes().any(|b| b.is_ascii_digit());
    match trimmed.parse::<f64>() {
        Ok(cpus) if plain => cpu_milli(cpus),
        _ => Err(format!(
            "`{text}` is not a CPU count: expected a number such as `4` or `0.5`"
        )),
    }
}

/// Convert a CPU count into thousandths of one logical CPU.
pub fn cpu_milli(cpus: f64) -> Result<u64, String> {
    if !cpus.is_finite() || cpus < 0.0 {
        return Err(format!(
            "`{cpus}` is not a CPU count: expected zero or more"
        ));
    }
    if cpus > MAX_CPUS {
        return Err(format!("`{cpus}` CPUs is more than any machine has"));
    }
    let milli = (cpus * 1000.0).round();
    if (milli - cpus * 1000.0).abs() > 1e-6 {
        return Err(format!("`{cpus}` is finer than a thousandth of a CPU"));
    }
    // In range by the checks above: 0 <= milli <= 1e9.
    Ok(milli as u64)
}

/// Render bytes exactly, in the largest unit that divides them: `8GiB`,
/// `1536MiB`, `300MB`, `1001B`.
pub fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0B".to_string();
    }
    let binary = BYTE_UNITS[5..].iter().rev();
    let decimal = BYTE_UNITS[1..5].iter().rev();
    for (unit, multiplier) in binary.chain(decimal) {
        if bytes.is_multiple_of(*multiplier) {
            return format!("{}{unit}", bytes / multiplier);
        }
    }
    format!("{bytes}B")
}

/// Render whole seconds as `1h30m`, `45s`, `0s`.
pub fn format_duration_secs(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    let mut out = String::new();
    let parts = [
        (secs / 3600, "h"),
        (secs % 3600 / 60, "m"),
        (secs % 60, "s"),
    ];
    for (value, unit) in parts {
        if value > 0 {
            out.push_str(&format!("{value}{unit}"));
        }
    }
    out
}

/// Render milli-CPUs as a CPU count: `4`, `0.5`, `1.25`.
pub fn format_cpu(milli: u64) -> String {
    let whole = milli / 1000;
    let fraction = milli % 1000;
    if fraction == 0 {
        return whole.to_string();
    }
    let digits = format!("{fraction:03}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

/// Render milli-CPUs as a phrase: `1 CPU`, `4 CPUs`, `0.5 CPUs`.
pub fn format_cpus(milli: u64) -> String {
    let unit = if milli == 1000 { "CPU" } else { "CPUs" };
    format!("{} {unit}", format_cpu(milli))
}

fn unit_list(units: &[(&str, u64)]) -> String {
    let names: Vec<&str> = units.iter().map(|(name, _)| *name).collect();
    match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => names.join(""),
    }
}

enum DecimalError {
    Malformed,
    Fractional,
    Overflow,
}

/// `number` (digits with at most one `.`) times `multiplier`, exactly.
fn scale_decimal(number: &str, multiplier: u64) -> Result<u64, DecimalError> {
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (number, None),
    };
    let digits_only = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits_only(whole) || fraction.is_some_and(|f| !digits_only(f)) {
        return Err(DecimalError::Malformed);
    }
    let multiplier = u128::from(multiplier);
    let whole: u128 = whole.parse().map_err(|_| DecimalError::Overflow)?;
    let mut total = whole
        .checked_mul(multiplier)
        .ok_or(DecimalError::Overflow)?;
    if let Some(fraction) = fraction.map(|f| f.trim_end_matches('0'))
        && !fraction.is_empty()
    {
        let exponent = u32::try_from(fraction.len()).map_err(|_| DecimalError::Fractional)?;
        let divisor = 10u128
            .checked_pow(exponent)
            .ok_or(DecimalError::Fractional)?;
        let numerator = fraction
            .parse::<u128>()
            .ok()
            .and_then(|f| f.checked_mul(multiplier))
            .ok_or(DecimalError::Fractional)?;
        if !numerator.is_multiple_of(divisor) {
            return Err(DecimalError::Fractional);
        }
        total = total
            .checked_add(numerator / divisor)
            .ok_or(DecimalError::Overflow)?;
    }
    u64::try_from(total).map_err(|_| DecimalError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_and_binary_sizes_differ() {
        assert_eq!(parse_bytes("1KB"), Ok(1000));
        assert_eq!(parse_bytes("1KiB"), Ok(1024));
        assert_eq!(parse_bytes("8GB"), Ok(8_000_000_000));
        assert_eq!(parse_bytes("8GiB"), Ok(8 << 30));
        assert_eq!(parse_bytes("2TiB"), Ok(2 << 40));
        assert_eq!(parse_bytes("1TB"), Ok(1_000_000_000_000));
        assert_eq!(parse_bytes("512MiB"), Ok(512 << 20));
        assert_eq!(parse_bytes("250MB"), Ok(250_000_000));
        assert_eq!(parse_bytes("4096B"), Ok(4096));
        assert_eq!(parse_bytes("0B"), Ok(0));
    }

    #[test]
    fn sizes_accept_fractions_spaces_and_any_case() {
        assert_eq!(parse_bytes("1.5GiB"), Ok(3 << 29));
        assert_eq!(parse_bytes("0.3GB"), Ok(300_000_000));
        assert_eq!(parse_bytes(" 8 GiB "), Ok(8 << 30));
        assert_eq!(parse_bytes("8gib"), Ok(8 << 30));
        assert_eq!(parse_bytes("8gb"), Ok(8_000_000_000));
        assert_eq!(parse_bytes("1.50KB"), Ok(1500));
    }

    #[test]
    fn size_errors_name_the_value() {
        let no_unit = parse_bytes("1024").unwrap_err();
        assert!(
            no_unit.contains("`1024`") && no_unit.contains("no unit"),
            "{no_unit}"
        );
        let bad_unit = parse_bytes("8 gigs").unwrap_err();
        assert!(
            bad_unit.contains("`8 gigs`") && bad_unit.contains("`gigs`"),
            "{bad_unit}"
        );
        assert!(bad_unit.contains("GiB or TiB"), "{bad_unit}");
        let ambiguous = parse_bytes("8G").unwrap_err();
        assert!(ambiguous.contains("unknown size unit `G`"), "{ambiguous}");
        let no_number = parse_bytes("GiB").unwrap_err();
        assert!(no_number.contains("`GiB`"), "{no_number}");
        let fractional = parse_bytes("0.1KiB").unwrap_err();
        assert!(fractional.contains("whole number of bytes"), "{fractional}");
        let overflow = parse_bytes("99999999999TiB").unwrap_err();
        assert!(overflow.contains("too large"), "{overflow}");
        assert!(parse_bytes("1.2.3GB").is_err());
        assert!(parse_bytes(".5GB").is_err());
        assert!(parse_bytes("-1GB").is_err());
        assert!(parse_bytes("").is_err());
    }

    #[test]
    fn durations_combine_units() {
        assert_eq!(parse_duration_ms("500ms"), Ok(500));
        assert_eq!(parse_duration_ms("45s"), Ok(45_000));
        assert_eq!(parse_duration_ms("30m"), Ok(1_800_000));
        assert_eq!(parse_duration_ms("4h"), Ok(14_400_000));
        assert_eq!(parse_duration_ms("1h30m"), Ok(5_400_000));
        assert_eq!(parse_duration_ms("1h 30m 15s"), Ok(5_415_000));
        assert_eq!(parse_duration_ms("1s500ms"), Ok(1500));
        assert_eq!(parse_duration_ms("2H"), Ok(7_200_000));
        assert_eq!(parse_duration_ms("0s"), Ok(0));
        assert_eq!(parse_duration_secs("1h30m"), Ok(5400));
        assert_eq!(parse_duration_secs("2000ms"), Ok(2));
    }

    #[test]
    fn duration_errors_name_the_value() {
        let no_unit = parse_duration_ms("30").unwrap_err();
        assert!(
            no_unit.contains("`30`") && no_unit.contains("no unit"),
            "{no_unit}"
        );
        let bad_unit = parse_duration_ms("3 days").unwrap_err();
        assert!(
            bad_unit.contains("`3 days`") && bad_unit.contains("`days`"),
            "{bad_unit}"
        );
        let order = parse_duration_ms("30m1h").unwrap_err();
        assert!(
            order.contains("`30m1h`") && order.contains("largest first"),
            "{order}"
        );
        let partial = parse_duration_ms("1h30").unwrap_err();
        assert!(partial.contains("no unit after `30`"), "{partial}");
        assert!(parse_duration_ms("1h1h").is_err());
        assert!(parse_duration_ms("1.5h").is_err());
        assert!(parse_duration_ms("m").is_err());
        assert!(parse_duration_ms("").is_err());
        assert!(parse_duration_ms("-5s").is_err());
        assert!(parse_duration_ms("99999999999999999999h").is_err());
        let sub_second = parse_duration_secs("1500ms").unwrap_err();
        assert!(sub_second.contains("`1500ms`") && sub_second.contains("whole number of seconds"));
    }

    #[test]
    fn cpu_counts_become_milli_cpus() {
        assert_eq!(parse_cpu("4"), Ok(4000));
        assert_eq!(parse_cpu("0.5"), Ok(500));
        assert_eq!(parse_cpu("1.25"), Ok(1250));
        assert_eq!(parse_cpu("0.001"), Ok(1));
        assert_eq!(cpu_milli(0.1), Ok(100));
        assert_eq!(cpu_milli(0.0), Ok(0));
        assert!(parse_cpu("0.0005").unwrap_err().contains("thousandth"));
        assert!(parse_cpu("four").unwrap_err().contains("`four`"));
        assert!(parse_cpu("1e3").is_err());
        assert!(parse_cpu("inf").is_err());
        assert!(parse_cpu("-1").is_err());
        assert!(cpu_milli(-1.0).is_err());
        assert!(cpu_milli(f64::NAN).is_err());
        assert!(cpu_milli(2_000_000.0).is_err());
    }

    #[test]
    fn formatting_round_trips() {
        for bytes in [0, 1, 1000, 1024, 1500, 8 << 30, 3 << 29, 300_000_000, 1001] {
            assert_eq!(parse_bytes(&format_bytes(bytes)), Ok(bytes), "{bytes}");
        }
        assert_eq!(format_bytes(8 << 30), "8GiB");
        assert_eq!(format_bytes(3 << 29), "1536MiB");
        assert_eq!(format_bytes(300_000_000), "300MB");
        for secs in [0, 1, 59, 60, 90, 3600, 5400, 5415, 86_400] {
            assert_eq!(
                parse_duration_secs(&format_duration_secs(secs)),
                Ok(secs),
                "{secs}"
            );
        }
        assert_eq!(format_duration_secs(5400), "1h30m");
        for milli in [0, 1, 500, 1000, 1250, 16_000] {
            assert_eq!(parse_cpu(&format_cpu(milli)), Ok(milli), "{milli}");
        }
        assert_eq!(format_cpu(1250), "1.25");
        assert_eq!(format_cpus(1000), "1 CPU");
        assert_eq!(format_cpus(4000), "4 CPUs");
        assert_eq!(format_cpus(500), "0.5 CPUs");
    }
}

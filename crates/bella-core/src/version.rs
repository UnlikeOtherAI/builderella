//! Loose version matching for toolchain capabilities.
//!
//! Toolchain versions are not strict semver (`16.2`, `21.0.4+7`,
//! `1.89.0-nightly`), so this compares dotted numeric prefixes and ignores
//! any suffix after the first character that is neither a digit nor a dot.
//!
//! Requirement grammar, comma separated, all terms must hold:
//! `>=1.80`, `>1.80`, `<=17`, `<18`, `=16.2`, or a bare prefix such as `17`
//! (matches `17`, `17.0.9`, not `170`).

use std::cmp::Ordering;

fn parse(version: &str) -> Vec<u64> {
    let end = version
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(version.len());
    version[..end]
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u64>().unwrap_or(u64::MAX))
        .collect()
}

/// Compare two versions numerically; missing components count as zero.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

fn has_prefix(version: &str, prefix: &str) -> bool {
    let (v, p) = (parse(version), parse(prefix));
    !p.is_empty() && v.len() >= p.len() && v[..p.len()] == p[..]
}

/// Does `version` satisfy `requirement`? An empty requirement always holds.
pub fn satisfies(version: &str, requirement: &str) -> bool {
    requirement.split(',').map(str::trim).filter(|term| !term.is_empty()).all(|term| {
        let ops: [(&str, fn(Ordering) -> bool); 5] = [
            (">=", |o| o != Ordering::Less),
            ("<=", |o| o != Ordering::Greater),
            (">", |o| o == Ordering::Greater),
            ("<", |o| o == Ordering::Less),
            ("=", |o| o == Ordering::Equal),
        ];
        for (op, accept) in ops {
            if let Some(rest) = term.strip_prefix(op) {
                return accept(compare(version, rest.trim()));
            }
        }
        has_prefix(version, term)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons() {
        assert!(satisfies("1.89.0", ">=1.85"));
        assert!(!satisfies("1.84.1", ">=1.85"));
        assert!(satisfies("21.0.4+7", "21"));
        assert!(!satisfies("170.1", "17"));
        assert!(satisfies("16.2", ">=16, <17"));
        assert!(!satisfies("17.0", ">=16, <17"));
        assert!(satisfies("14.2", "=14.2.0"));
        assert!(satisfies("anything", ""));
        assert!(!satisfies("", "17"));
    }
}

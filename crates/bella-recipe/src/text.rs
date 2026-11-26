//! Rules for the individual strings of a recipe file: names, paths,
//! platforms and version requirements. Each check returns a message that
//! names the offending text.

use bella_core::platform::{Arch, CapabilityRequirement, Os, PlatformSelector};
use bella_core::snapshot::validate_rel_path;
use std::collections::BTreeMap;

const MAX_NAME_LEN: usize = 64;

/// The candidate closest to `word`, if it is close enough to be a typo.
pub(crate) fn closest<'c>(word: &str, candidates: &[&'c str]) -> Option<&'c str> {
    let length = word.chars().count();
    // Two edits cover a transposition, the commonest typo; very short words
    // get one so that unrelated keys are not suggested.
    let budget = if length <= 3 { 1 } else { (length / 3).max(2) };
    candidates
        .iter()
        .map(|candidate| (edit_distance(word, candidate), *candidate))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = diagonal + usize::from(ca != *cb);
            diagonal = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

/// "`a`, `b` or `c`".
pub(crate) fn one_of(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => quoted.concat(),
    }
}

/// Recipe and step names: `[a-z0-9_-]+`.
pub(crate) fn check_name(name: &str, what: &str) -> Result<(), String> {
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-';
    if name.is_empty() || !name.bytes().all(allowed) {
        return Err(format!(
            "`{name}` is not a valid {what} name: use lowercase letters, digits, `-` and `_`"
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "{what} name `{name}` is longer than {MAX_NAME_LEN} characters"
        ));
    }
    Ok(())
}

/// Project names appear in URLs and on the dashboard.
pub(crate) fn check_project_name(name: &str) -> Result<(), String> {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.');
    let starts_well = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric());
    if !starts_well || !name.bytes().all(allowed) {
        return Err(format!(
            "`{name}` is not a valid project name: use letters, digits, `-`, `_` and `.`, \
             starting with a letter or digit"
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "project name `{name}` is longer than {MAX_NAME_LEN} characters"
        ));
    }
    Ok(())
}

/// Capture patterns are interpreted by the capture step; here they only
/// have to be something.
pub(crate) fn check_pattern(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("a pattern must not be empty".to_string());
    }
    if pattern.contains('\0') {
        return Err(format!(
            "`{}` contains a NUL character",
            pattern.escape_debug()
        ));
    }
    Ok(())
}

/// Where an extra source root appears inside the snapshot.
pub(crate) fn check_mount(mount: &str) -> Result<(), String> {
    if mount.contains('\\') {
        return Err(format!(
            "mount `{mount}` uses `\\`: separate directories with `/`"
        ));
    }
    validate_rel_path(mount).map_err(|reason| {
        format!("`{mount}` is not a valid mount path ({reason}): use a relative path such as `libs/shared`")
    })
}

/// Where an extra source root is on disk, relative to the recipe file.
pub(crate) fn check_root_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("the path of a source root must not be empty".to_string());
    }
    if path.contains('\0') {
        return Err(format!(
            "`{}` contains a NUL character",
            path.escape_debug()
        ));
    }
    if is_absolute(path) {
        return Err(format!(
            "`{path}` is an absolute path: give the source root relative to this file, such as \
             `../shared-lib`"
        ));
    }
    Ok(())
}

/// Artifact and test-report globs, relative to the source set root.
pub(crate) fn check_glob(glob: &str) -> Result<(), String> {
    if glob.trim().is_empty() {
        return Err("a path pattern must not be empty".to_string());
    }
    if glob.contains('\0') {
        return Err(format!(
            "`{}` contains a NUL character",
            glob.escape_debug()
        ));
    }
    if glob.contains('\\') {
        return Err(format!("`{glob}` uses `\\`: separate directories with `/`"));
    }
    if is_absolute(glob) {
        return Err(format!(
            "`{glob}` is an absolute path: outputs are collected relative to the source root"
        ));
    }
    if glob.split('/').any(|component| component == "..") {
        return Err(format!(
            "`{glob}` contains `..`: outputs are collected from inside the source root only"
        ));
    }
    Ok(())
}

/// A step's working directory: relative to the source set root, staying
/// inside it. `.` is the root itself and yields `None`.
pub(crate) fn working_directory(cwd: &str) -> Result<Option<String>, String> {
    if cwd.contains('\\') {
        return Err(format!("`{cwd}` uses `\\`: separate directories with `/`"));
    }
    if is_absolute(cwd) {
        return Err(format!(
            "`{cwd}` is an absolute path: `cwd` is relative to the source root"
        ));
    }
    let trimmed = cwd.strip_suffix('/').unwrap_or(cwd);
    if trimmed == "." {
        return Ok(None);
    }
    if trimmed.split('/').any(|component| component == "..") {
        return Err(format!(
            "`{cwd}` contains `..`: `cwd` must stay inside the source root"
        ));
    }
    validate_rel_path(trimmed)
        .map_err(|reason| format!("`{cwd}` is not a valid working directory ({reason})"))?;
    Ok(Some(trimmed.to_string()))
}

fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

pub(crate) fn check_env_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("an environment variable needs a name".to_string());
    }
    if name.contains('=') || name.contains('\0') || name.chars().any(char::is_whitespace) {
        return Err(format!(
            "`{}` is not a valid environment variable name: it must not contain `=` or spaces",
            name.escape_debug()
        ));
    }
    Ok(())
}

pub(crate) fn check_env_value(value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err("an environment value must not contain a NUL character".to_string());
    }
    Ok(())
}

fn is_identifier(name: &str, extra: &[u8]) -> bool {
    let allowed = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || extra.contains(&b);
    name.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(allowed)
}

/// Capability names as discovered by the workers: `rust`, `android-sdk`.
pub(crate) fn check_capability_name(name: &str) -> Result<(), String> {
    if is_identifier(name, b"-_.+") {
        return Ok(());
    }
    Err(format!(
        "`{name}` is not a capability name: use lowercase letters, digits, `-`, `_`, `.` and `+`"
    ))
}

/// Token names: `gpu`, `ios-simulator`, `device:pixel-8`.
pub(crate) fn check_token_name(name: &str) -> Result<(), String> {
    if is_identifier(name, b"-_.:") {
        return Ok(());
    }
    Err(format!(
        "`{name}` is not a token name: use lowercase letters, digits, `-`, `_`, `.` and `:`"
    ))
}

pub(crate) fn check_attribute(name: &str) -> Result<(), String> {
    if is_identifier(name, b"-_.") {
        return Ok(());
    }
    Err(format!(
        "`{name}` is not an attribute name: use lowercase letters, digits, `-`, `_` and `.`"
    ))
}

pub(crate) fn check_http_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "`{}` is not an HTTP path: it must start with `/` and contain no spaces",
            path.escape_debug()
        ));
    }
    Ok(())
}

/// `any` leaves the operating system open.
pub(crate) fn os_from_str(text: &str) -> Result<Option<Os>, String> {
    if text == "any" {
        return Ok(None);
    }
    text.parse::<Os>().map(Some).map_err(|_| {
        format!("unknown operating system `{text}`: use `linux`, `macos`, `windows` or `any`")
    })
}

/// `any` leaves the architecture open.
pub(crate) fn arch_from_str(text: &str) -> Result<Option<Arch>, String> {
    if text == "any" {
        return Ok(None);
    }
    text.parse::<Arch>()
        .map(Some)
        .map_err(|_| format!("unknown architecture `{text}`: use `x86_64`, `aarch64` or `any`"))
}

/// `linux`, `macos-aarch64`, `any-x86_64`, `any`.
pub(crate) fn selector_from_str(text: &str) -> Result<PlatformSelector, String> {
    let (os, arch) = match text.split_once('-') {
        Some((os, arch)) => (os, Some(arch)),
        None => (text, None),
    };
    let wrap = |message: String| format!("`{text}` is not a platform: {message}");
    Ok(PlatformSelector {
        os: os_from_str(os).map_err(wrap)?,
        arch: match arch {
            Some(arch) => arch_from_str(arch).map_err(wrap)?,
            None => None,
        },
        os_version: None,
    })
}

/// Could one machine match both selectors? Version requirements are not
/// compared; only a definite conflict of OS or architecture counts.
pub(crate) fn selectors_overlap(a: &PlatformSelector, b: &PlatformSelector) -> bool {
    let os_conflict = matches!((a.os, b.os), (Some(x), Some(y)) if x != y);
    let arch_conflict = matches!((a.arch, b.arch), (Some(x), Some(y)) if x != y);
    !os_conflict && !arch_conflict
}

/// Check a version requirement and put it in the form `bella_core::version`
/// matches: comma-separated terms, each `>=V`, `>V`, `<=V`, `<V` or `V`.
///
/// In a recipe `=V` and a bare `V` both mean "version `V` at the precision
/// written": `22` and `=22` accept 22.0 and 22.11.3, `=22.11` accepts
/// 22.11.x only. Both become the bare form.
pub(crate) fn version_requirement(text: &str) -> Result<String, String> {
    let mut terms = Vec::new();
    for term in text.split(',') {
        let term = term.trim();
        let (operator, version) = [">=", "<=", ">", "<", "="]
            .iter()
            .find_map(|op| term.strip_prefix(op).map(|rest| (*op, rest.trim())))
            .unwrap_or(("", term));
        let well_formed = version.starts_with(|c: char| c.is_ascii_digit())
            && version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-' | b'_'));
        if !well_formed {
            return Err(format!(
                "`{text}` is not a version requirement: write versions such as `22`, `>=1.85` \
                 or `>=16,<17`"
            ));
        }
        let operator = if operator == "=" { "" } else { operator };
        terms.push(format!("{operator}{version}"));
    }
    Ok(terms.join(","))
}

/// `rust`, `rust>=1.85`, `node=22`, `xcode >=16,<17`.
pub(crate) fn requirement_from_str(text: &str) -> Result<CapabilityRequirement, String> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| matches!(c, '<' | '>' | '=') || c.is_whitespace())
        .unwrap_or(trimmed.len());
    let (name, rest) = (&trimmed[..split], trimmed[split..].trim());
    check_capability_name(name)
        .map_err(|message| format!("`{text}` is not a requirement: {message}"))?;
    let version = if rest.is_empty() {
        None
    } else {
        Some(version_requirement(rest)?)
    };
    Ok(CapabilityRequirement {
        name: name.to_string(),
        version,
        attributes: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bella_core::version::satisfies;

    #[test]
    fn typos_get_a_suggestion_and_unrelated_words_do_not() {
        let keys = ["memory", "timeout", "max_memory", "cpu"];
        assert_eq!(closest("memroy", &keys), Some("memory"));
        assert_eq!(closest("timeuot", &keys), Some("timeout"));
        assert_eq!(closest("cpus", &keys), Some("cpu"));
        assert_eq!(closest("banana", &keys), None);
        assert_eq!(one_of(&["a", "b", "c"]), "`a`, `b` or `c`");
        assert_eq!(one_of(&["a"]), "`a`");
    }

    #[test]
    fn names() {
        assert!(check_name("unit-tests_2", "step").is_ok());
        for bad in ["", "Build", "has space", "é", "a.b", "a/b"] {
            let message = check_name(bad, "step").unwrap_err();
            assert!(message.contains(&format!("`{bad}`")), "{message}");
        }
        assert!(check_name(&"a".repeat(65), "step").is_err());
        assert!(check_project_name("My.App-2").is_ok());
        assert!(check_project_name("-app").is_err());
        assert!(check_project_name("my app").is_err());
        assert!(check_project_name("").is_err());
    }

    #[test]
    fn working_directories_stay_inside_the_source_root() {
        assert_eq!(working_directory("."), Ok(None));
        assert_eq!(working_directory("./"), Ok(None));
        assert_eq!(working_directory("web"), Ok(Some("web".to_string())));
        assert_eq!(
            working_directory("web/app/"),
            Ok(Some("web/app".to_string()))
        );
        for bad in [
            "..",
            "../x",
            "a/../b",
            "/abs",
            "C:/x",
            "C:\\x",
            "\\\\server\\share",
            "a//b",
            "",
            "./a",
        ] {
            let message = working_directory(bad).unwrap_err();
            assert!(message.contains(&format!("`{bad}`")), "{bad}: {message}");
        }
    }

    #[test]
    fn output_globs_stay_inside_the_source_root() {
        assert!(check_glob("target/release/app").is_ok());
        assert!(check_glob("**/junit.xml").is_ok());
        assert!(check_glob("dist/**").is_ok());
        for bad in ["", "/etc/passwd", "../secrets", "a/../../b", "C:/x", "a\\b"] {
            assert!(check_glob(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn capture_roots() {
        assert!(check_mount("libs/shared").is_ok());
        for bad in [".", "", "/abs", "a/../b", "a\\b", "a//b"] {
            assert!(check_mount(bad).is_err(), "{bad}");
        }
        assert!(check_root_path("../shared-lib").is_ok());
        assert!(check_root_path("vendor/lib").is_ok());
        for bad in ["", "/abs/lib", "C:\\lib", "D:/lib"] {
            assert!(check_root_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn platform_strings() {
        assert_eq!(
            selector_from_str("linux"),
            Ok(PlatformSelector::os(Os::Linux))
        );
        assert_eq!(
            selector_from_str("macos-aarch64"),
            Ok(PlatformSelector {
                os: Some(Os::Macos),
                arch: Some(Arch::Aarch64),
                os_version: None
            })
        );
        assert_eq!(
            selector_from_str("darwin-arm64"),
            selector_from_str("macos-aarch64")
        );
        assert_eq!(selector_from_str("any"), Ok(PlatformSelector::default()));
        assert_eq!(
            selector_from_str("any-x86_64").unwrap(),
            PlatformSelector {
                os: None,
                arch: Some(Arch::X86_64),
                os_version: None
            }
        );
        let unknown_os = selector_from_str("plan9").unwrap_err();
        assert!(
            unknown_os.contains("`plan9`") && unknown_os.contains("`linux`"),
            "{unknown_os}"
        );
        let unknown_arch = selector_from_str("linux-riscv64").unwrap_err();
        assert!(unknown_arch.contains("`riscv64`"), "{unknown_arch}");
        assert!(selector_from_str("").is_err());
        assert!(selector_from_str("x86_64").is_err());
    }

    #[test]
    fn selector_overlap_only_fails_on_a_definite_conflict() {
        let linux = selector_from_str("linux").unwrap();
        let linux_arm = selector_from_str("linux-aarch64").unwrap();
        let linux_x86 = selector_from_str("linux-x86_64").unwrap();
        let mac = selector_from_str("macos").unwrap();
        let any = selector_from_str("any").unwrap();
        assert!(selectors_overlap(&linux, &linux_arm));
        assert!(selectors_overlap(&any, &mac));
        assert!(!selectors_overlap(&linux_x86, &linux_arm));
        assert!(!selectors_overlap(&mac, &linux_arm));
    }

    #[test]
    fn version_requirements_match_what_the_scheduler_evaluates() {
        assert_eq!(version_requirement(">=1.85"), Ok(">=1.85".to_string()));
        assert_eq!(
            version_requirement(" >= 16 , < 17 "),
            Ok(">=16,<17".to_string())
        );
        assert_eq!(version_requirement("22"), Ok("22".to_string()));
        // `=22` means "a 22 release", not "exactly 22.0.0".
        let node = version_requirement("=22").unwrap();
        assert_eq!(node, "22");
        assert!(satisfies("22.11.0", &node));
        assert!(!satisfies("23.0.0", &node));
        assert!(!satisfies("220.1", &node));
        let exact = version_requirement("=22.11").unwrap();
        assert!(satisfies("22.11.3", &exact));
        assert!(!satisfies("22.12.0", &exact));
        for bad in ["", "==22", ">=", "latest", ">=1.85,", "^1.2", "~1.2", "1 2"] {
            let message = version_requirement(bad).unwrap_err();
            assert!(message.contains(&format!("`{bad}`")), "{bad}: {message}");
        }
    }

    #[test]
    fn requirement_shorthand() {
        let rust = requirement_from_str("rust>=1.85").unwrap();
        assert_eq!(
            (rust.name.as_str(), rust.version.as_deref()),
            ("rust", Some(">=1.85"))
        );
        let node = requirement_from_str("node=22").unwrap();
        assert_eq!(
            (node.name.as_str(), node.version.as_deref()),
            ("node", Some("22"))
        );
        let spaced = requirement_from_str("xcode >=16, <17").unwrap();
        assert_eq!(spaced.version.as_deref(), Some(">=16,<17"));
        let bare = requirement_from_str("android-sdk").unwrap();
        assert_eq!((bare.name.as_str(), bare.version), ("android-sdk", None));
        assert_eq!(
            requirement_from_str("jdk 21").unwrap().version.as_deref(),
            Some("21")
        );
        for bad in ["", ">=1.85", "Rust", "rust>=", "rust==1.85", "rust latest"] {
            assert!(requirement_from_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn identifiers() {
        assert!(check_token_name("device:pixel-8").is_ok());
        assert!(check_token_name("ios-simulator").is_ok());
        assert!(check_token_name("GPU").is_err());
        assert!(check_token_name("").is_err());
        assert!(check_capability_name("c++").is_ok());
        assert!(check_capability_name("device:x").is_err());
        assert!(check_env_name("RUST_BACKTRACE").is_ok());
        assert!(check_env_name("A=B").is_err());
        assert!(check_env_name("").is_err());
        assert!(check_env_name("WITH SPACE").is_err());
        assert!(check_http_path("/health").is_ok());
        assert!(check_http_path("health").is_err());
        assert!(check_http_path("/a b").is_err());
    }
}

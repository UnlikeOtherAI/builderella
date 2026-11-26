//! Helpers shared by the integration tests.
#![allow(dead_code)]

use bella_recipe::{Issue, RecipeFile, Severity, parse};

pub const GIB: u64 = 1 << 30;
pub const MIB: u64 = 1 << 20;

pub fn render(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(Issue::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse text that must be a valid recipe file.
pub fn valid(text: &str) -> RecipeFile {
    parse(text).unwrap_or_else(|issues| panic!("expected a valid file, got:\n{}", render(&issues)))
}

/// Parse text that must be refused; returns every issue.
pub fn invalid(text: &str) -> Vec<Issue> {
    match parse(text) {
        Ok(_) => panic!("expected the file to be refused:\n{text}"),
        Err(issues) => {
            assert!(
                issues.iter().any(Issue::is_error),
                "a refused file must carry an error"
            );
            issues
        }
    }
}

pub fn errors(issues: &[Issue]) -> Vec<&Issue> {
    issues.iter().filter(|i| i.is_error()).collect()
}

/// The single issue at `path`.
pub fn at<'a>(issues: &'a [Issue], path: &str) -> &'a Issue {
    let found: Vec<&Issue> = issues.iter().filter(|i| i.path == path).collect();
    match found.as_slice() {
        [one] => one,
        _ => panic!(
            "expected one issue at `{path}`, found {}:\n{}",
            found.len(),
            render(issues)
        ),
    }
}

/// The single error in a file that is wrong in exactly one way.
pub fn only_error(text: &str) -> Issue {
    let issues = invalid(text);
    let found = errors(&issues);
    assert_eq!(
        found.len(),
        1,
        "expected one error, got:\n{}",
        render(&issues)
    );
    found[0].clone()
}

pub fn warning_at<'a>(issues: &'a [Issue], path: &str) -> &'a Issue {
    let issue = at(issues, path);
    assert_eq!(issue.severity, Severity::Warning, "{issue}");
    issue
}

/// 1-based line of the first line containing `needle`.
pub fn line_of(text: &str, needle: &str) -> u32 {
    let index = text
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` is not in the text"));
    u32::try_from(index + 1).unwrap()
}

/// 1-based column, in characters, at which `needle` starts on its line.
pub fn column_of(text: &str, needle: &str) -> u32 {
    let line = text
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("`{needle}` is not in the text"));
    let bytes = line.find(needle).unwrap();
    u32::try_from(line[..bytes].chars().count() + 1).unwrap()
}

/// A complete file with one recipe named `r` whose body is `body`.
pub fn recipe(body: &str) -> String {
    format!("[project]\nname = \"demo\"\n\n[recipes.r]\n{body}\n")
}

/// A complete file whose recipe `r` consists of the given step tables.
pub fn with_steps(steps: &str) -> String {
    format!("[project]\nname = \"demo\"\n\n{steps}\n")
}

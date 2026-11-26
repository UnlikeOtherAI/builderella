//! `docs/recipes.md` is the reference for the file format. Every example
//! in it is parsed here, so the page cannot drift from the parser.

mod common;

use bella_recipe::{Overrides, PolicyLimits, parse, resolve};
use common::*;

const DOCS: &str = include_str!("../../../docs/recipes.md");

struct Block {
    /// The text after the opening fence: `toml`, `toml invalid`, `text`.
    info: String,
    text: String,
    /// Line of the opening fence, for messages.
    line: usize,
}

fn blocks() -> Vec<Block> {
    let mut out = Vec::new();
    let mut open: Option<Block> = None;
    for (index, line) in DOCS.lines().enumerate() {
        match (open.take(), line.strip_prefix("```")) {
            (None, Some(info)) => {
                open = Some(Block {
                    info: info.trim().to_string(),
                    text: String::new(),
                    line: index + 1,
                });
            }
            (Some(block), Some(rest)) => {
                assert!(rest.is_empty(), "line {}: unexpected fence", index + 1);
                out.push(block);
            }
            (Some(mut block), None) => {
                block.text.push_str(line);
                block.text.push('\n');
                open = Some(block);
            }
            (None, None) => {}
        }
    }
    assert!(open.is_none(), "unterminated code block");
    out
}

fn examples() -> Vec<Block> {
    blocks().into_iter().filter(|b| b.info == "toml").collect()
}

#[test]
fn every_example_is_a_valid_file_without_warnings() {
    let examples = examples();
    assert!(examples.len() >= 6, "found {} examples", examples.len());
    for block in &examples {
        let file = parse(&block.text).unwrap_or_else(|issues| {
            panic!(
                "the example at line {} of docs/recipes.md is not valid:\n{}",
                block.line,
                render(&issues)
            )
        });
        assert!(
            file.warnings.is_empty(),
            "the example at line {} has warnings:\n{}",
            block.line,
            render(&file.warnings)
        );
        // Each recipe also resolves into a job.
        for name in file.recipe_names() {
            let limits = PolicyLimits::unrestricted();
            let resolved =
                resolve(&file, name, &Overrides::default(), &limits).unwrap_or_else(|issues| {
                    panic!(
                        "recipe `{name}` at line {} does not resolve:\n{}",
                        block.line,
                        render(&issues)
                    )
                });
            assert!(resolved.spec.envelope.is_well_formed());
            assert!(resolved.warnings.is_empty());
        }
    }
}

#[test]
fn the_page_has_a_small_and_a_full_example() {
    let examples = examples();

    let small = valid(&examples[0].text);
    assert!(examples[0].text.lines().count() <= 10);
    assert_eq!(small.recipe_names().collect::<Vec<_>>(), ["test"]);
    assert_eq!(small.recipes["test"].steps.len(), 1);

    let last = examples.last().unwrap();
    let full = valid(&last.text);
    assert_eq!(full.recipe_names().collect::<Vec<_>>(), ["e2e", "test"]);
    assert!(!full.capture.roots.is_empty());
    assert!(full.recipes["e2e"].environment.is_some());

    // "Every key of the format in one file."
    let keys = [
        // [project], [capture]
        "name",
        "exclude",
        "include_ignored",
        "secrets",
        "allow_secrets",
        "roots",
        // recipe
        "description",
        "run",
        "shell",
        "cwd",
        "env",
        "hosts",
        "target",
        "test_runtime",
        "requires",
        "isolation",
        "cpu",
        "memory",
        "disk",
        "tokens",
        "max_cpu",
        "max_memory",
        "max_disk",
        "parallelism",
        "priority",
        "timeout",
        "cleanup_timeout",
        "retry",
        "max_attempts",
        "cloud",
        "lan_only",
        "disconnected",
        "artifacts",
        "test_reports",
        // step
        "kind",
        "needs",
        "ports",
        "ready",
        // ready
        "tcp",
        "http",
        "path",
        "status",
        "interval",
        // environment
        "idle_timeout",
        "lease",
        "max_lifetime",
        // platform and requirement tables
        "os",
        "arch",
        "os_version",
        "version",
        "attributes",
    ];
    // Everything above the full example is the reference proper.
    let reference = &DOCS[..DOCS.find("## A full example").unwrap()];
    for key in keys {
        let used = last
            .text
            .lines()
            .any(|line| line.contains(&format!("{key} = ")));
        assert!(used, "the full example does not use `{key}`");
        assert!(
            reference.contains(&format!("`{key}`")) || reference.contains(&format!("{key} = ")),
            "`{key}` is not explained before the full example"
        );
    }
    assert!(last.text.contains("[[recipes.e2e.steps]]"));
    assert!(last.text.contains("[recipes.e2e.environment]"));
    assert!(last.text.contains("[defaults]"));
    // The probes that the full example does not use are documented too.
    for probe in ["{ log = ", "{ run = "] {
        assert!(DOCS.contains(probe), "`{probe}` is not shown on the page");
    }
}

#[test]
fn the_error_example_shows_the_real_messages() {
    let all = blocks();
    let position = all
        .iter()
        .position(|b| b.info == "toml invalid")
        .expect("the page shows an invalid file");
    let issues = invalid(&all[position].text);
    let shown = &all[position + 1];
    assert_eq!(shown.info, "text");
    assert_eq!(shown.text.trim_end(), render(&issues));
    assert_eq!(
        all.iter().filter(|b| b.info == "toml invalid").count(),
        1,
        "each invalid example needs its own check"
    );
}

#[test]
fn the_numbers_quoted_in_the_prose_are_right() {
    // "This job reserves 9 CPUs and 7 GiB".
    let steps = examples()
        .into_iter()
        .find(|b| b.text.contains("collect-logs") && !b.text.contains("[capture]"))
        .expect("the steps example");
    let peak = valid(&steps.text).recipes["e2e"].envelope.peak.clone();
    assert_eq!((peak.cpu_milli, peak.memory_bytes), (9000, 7 * GIB));
    assert!(DOCS.contains("reserves 9 CPUs and 7 GiB"));

    // "`"8GB"` is 8,000,000,000 bytes and `"8GiB"` is 8,589,934,592".
    assert_eq!(bella_recipe::units::parse_bytes("8GB"), Ok(8_000_000_000));
    assert_eq!(bella_recipe::units::parse_bytes("8GiB"), Ok(8_589_934_592));
    assert!(DOCS.contains("8,000,000,000") && DOCS.contains("8,589,934,592"));
}

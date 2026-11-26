//! Reading and validating `builderella.toml`.
//!
//! The file is parsed into a span-preserving TOML document and then walked
//! by hand. A derived deserializer would stop at the first problem; walking
//! lets every unknown key, bad unit and broken step reference be reported
//! in one pass, each with its position.

use crate::graph;
use crate::issue::{Issue, LineIndex, Pos, Severity, sort_issues};
use crate::model::{CaptureSection, ProjectSection, Recipe, RecipeFile};
use crate::text::{self, closest};
use crate::units;
use bella_core::job::{
    CloudMode, CommandSpec, LeaseSpec, Priority, Readiness, ReadinessProbe, RetryClass, StepKind,
    StepSpec,
};
use bella_core::platform::{
    CapabilityRequirement, Compatibility, IsolationLevel, PlatformSelector,
};
use bella_core::resources::{ResourceEnvelope, ResourceVector};
use std::collections::BTreeMap;
use std::ops::Range;
use toml::Spanned;
use toml::de::{DeTable, DeValue};

/// Name given to the single step of a recipe that uses the `run` shorthand.
pub const MAIN_STEP: &str = "main";

/// More steps than this in one recipe is refused.
pub const MAX_STEPS: usize = 256;

/// How long a service may take to become ready when `ready.timeout` is not set.
pub const DEFAULT_READY_TIMEOUT_SECS: u64 = 60;

/// How often a readiness probe is retried when `ready.interval` is not set.
pub const DEFAULT_READY_INTERVAL_MS: u64 = 500;

/// Parse and validate the text of a `builderella.toml`.
///
/// On success every recipe in the returned file is complete and consistent:
/// its steps are ordered, its envelope is well formed and `[defaults]` are
/// merged in. Warnings are in [`RecipeFile::warnings`].
///
/// On failure the list holds **every** problem found, errors and warnings,
/// ordered by position. If the text is not valid TOML only the syntax
/// errors are returned, because nothing else can be trusted.
pub fn parse(text: &str) -> Result<RecipeFile, Vec<Issue>> {
    let index = LineIndex::new(text);
    let (document, syntax_errors) = DeTable::parse_recoverable(text);
    if !syntax_errors.is_empty() {
        let mut issues: Vec<Issue> = syntax_errors
            .iter()
            .map(|error| {
                let pos = error.span().map(|span| index.pos(span.start));
                let message = format!("invalid TOML: {}", error.message());
                Issue::new(Severity::Error, "", pos, message)
            })
            .collect();
        sort_issues(&mut issues);
        issues.dedup();
        return Err(issues);
    }

    let mut ctx = Ctx {
        index,
        issues: Vec::new(),
        positions: BTreeMap::new(),
    };
    let (project, capture, recipes) = document_sections(&mut ctx, document.get_ref());
    let Ctx {
        mut issues,
        positions,
        ..
    } = ctx;
    sort_issues(&mut issues);
    match project {
        Some(project) if !issues.iter().any(Issue::is_error) => Ok(RecipeFile {
            project,
            capture,
            recipes,
            warnings: issues,
            positions,
        }),
        _ => Err(issues),
    }
}

// ---------------------------------------------------------------- walking

type Span = Range<usize>;

struct Ctx<'t> {
    index: LineIndex<'t>,
    issues: Vec<Issue>,
    positions: BTreeMap<String, Pos>,
}

impl Ctx<'_> {
    /// A value at `path`. Its position is remembered so that later problems
    /// can be reported by path alone.
    fn field<'a, 'i>(&mut self, path: String, value: &'a Spanned<DeValue<'i>>) -> Field<'a, 'i> {
        let pos = self.index.pos(value.span().start);
        self.positions.insert(path.clone(), pos);
        Field { path, value }
    }

    fn push(&mut self, severity: Severity, path: &str, pos: Option<Pos>, message: String) {
        self.issues.push(Issue::new(severity, path, pos, message));
    }

    fn error(&mut self, path: &str, message: impl Into<String>) {
        let pos = self.positions.get(path).copied();
        self.push(Severity::Error, path, pos, message.into());
    }

    fn warn(&mut self, path: &str, message: impl Into<String>) {
        let pos = self.positions.get(path).copied();
        self.push(Severity::Warning, path, pos, message.into());
    }

    fn error_at(&mut self, path: &str, span: &Span, message: impl Into<String>) {
        let pos = self.index.pos(span.start);
        self.push(Severity::Error, path, Some(pos), message.into());
    }

    fn error_count(&self) -> usize {
        self.issues.iter().filter(|i| i.is_error()).count()
    }
}

struct Field<'a, 'i> {
    path: String,
    value: &'a Spanned<DeValue<'i>>,
}

/// A table whose keys are taken one by one; whatever is left is unknown.
struct Table<'a, 'i> {
    path: String,
    entries: BTreeMap<&'a str, (Span, &'a Spanned<DeValue<'i>>)>,
    /// Every key that was asked for, used to suggest a fix for a typo.
    known: Vec<&'static str>,
}

impl<'a, 'i> Table<'a, 'i> {
    fn new(path: &str, table: &'a DeTable<'i>) -> Self {
        let entries = table
            .iter()
            .map(|(key, value)| (&**key.get_ref(), (key.span(), value)))
            .collect();
        Self {
            path: path.to_string(),
            entries,
            known: Vec::new(),
        }
    }

    fn take(&mut self, ctx: &mut Ctx<'_>, key: &'static str) -> Option<Field<'a, 'i>> {
        self.known.push(key);
        let (_, value) = self.entries.remove(key)?;
        Some(ctx.field(child_path(&self.path, key), value))
    }

    /// Report every key that nothing asked for.
    fn finish(self, ctx: &mut Ctx<'_>) {
        for (key, (span, _)) in &self.entries {
            let hint = match closest(key, &self.known) {
                Some(known) => format!("; did you mean `{known}`?"),
                None => String::new(),
            };
            ctx.error_at(
                &child_path(&self.path, key),
                span,
                format!("unknown key `{key}`{hint}"),
            );
        }
    }

    /// All entries, for tables whose keys are names chosen by the author.
    fn into_fields(self, ctx: &mut Ctx<'_>) -> Vec<(&'a str, Span, Field<'a, 'i>)> {
        let path = self.path;
        self.entries
            .into_iter()
            .map(|(key, (span, value))| (key, span, ctx.field(child_path(&path, key), value)))
            .collect()
    }
}

fn child_path(parent: &str, key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let key = if bare {
        key.to_string()
    } else {
        format!("\"{key}\"")
    };
    if parent.is_empty() {
        key
    } else {
        format!("{parent}.{key}")
    }
}

fn describe(value: &DeValue<'_>) -> &'static str {
    match value {
        DeValue::String(_) => "a string",
        DeValue::Integer(_) => "an integer",
        DeValue::Float(_) => "a float",
        DeValue::Boolean(_) => "a boolean",
        DeValue::Datetime(_) => "a date",
        DeValue::Array(_) => "an array",
        DeValue::Table(_) => "a table",
    }
}

fn type_error(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, expected: &str) {
    let found = describe(f.value.get_ref());
    ctx.error(&f.path, format!("expected {expected}, found {found}"));
}

/// Report `result`'s error at the field and turn it into an `Option`.
fn checked<T>(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, result: Result<T, String>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(message) => {
            ctx.error(&f.path, message);
            None
        }
    }
}

fn as_str<'a>(ctx: &mut Ctx<'_>, f: &Field<'a, '_>) -> Option<&'a str> {
    let value: &'a Spanned<DeValue<'_>> = f.value;
    match value.get_ref() {
        DeValue::String(s) => Some(&**s),
        _ => {
            type_error(ctx, f, "a string");
            None
        }
    }
}

/// A string that must contain something.
fn as_text(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<String> {
    let s = as_str(ctx, f)?;
    if s.trim().is_empty() {
        ctx.error(&f.path, "must not be empty");
        return None;
    }
    Some(s.to_string())
}

fn as_bool(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<bool> {
    match f.value.get_ref() {
        DeValue::Boolean(b) => Some(*b),
        _ => {
            type_error(ctx, f, "`true` or `false`");
            None
        }
    }
}

/// A whole number in `min..=max`. `what` completes "expected ...".
fn as_int(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, what: &str, min: u64, max: u64) -> Option<u64> {
    let DeValue::Integer(integer) = f.value.get_ref() else {
        type_error(ctx, f, what);
        return None;
    };
    let parsed = i128::from_str_radix(integer.as_str(), integer.radix()).ok();
    match parsed.and_then(|v| u64::try_from(v).ok()) {
        Some(value) if (min..=max).contains(&value) => Some(value),
        _ => {
            let range = if max == u64::MAX {
                format!("{min} or more")
            } else {
                format!("{min} to {max}")
            };
            ctx.error(
                &f.path,
                format!("`{integer}` is out of range: expected {what}, {range}"),
            );
            None
        }
    }
}

fn as_u32(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, what: &str, min: u32) -> Option<u32> {
    let value = as_int(ctx, f, what, u64::from(min), u64::from(u32::MAX))?;
    u32::try_from(value).ok()
}

fn as_array<'a, 'i>(
    ctx: &mut Ctx<'_>,
    f: &Field<'a, 'i>,
    expected: &str,
) -> Option<&'a [Spanned<DeValue<'i>>]> {
    let value: &'a Spanned<DeValue<'i>> = f.value;
    match value.get_ref() {
        DeValue::Array(array) => Some(&array[..]),
        _ => {
            type_error(ctx, f, expected);
            None
        }
    }
}

fn as_table<'a, 'i>(ctx: &mut Ctx<'_>, f: &Field<'a, 'i>) -> Option<Table<'a, 'i>> {
    let value: &'a Spanned<DeValue<'i>> = f.value;
    match value.get_ref() {
        DeValue::Table(table) => Some(Table::new(&f.path, table)),
        _ => {
            type_error(ctx, f, "a table");
            None
        }
    }
}

/// The elements of an array as fields `path[0]`, `path[1]`, ...
fn elements<'a, 'i>(
    ctx: &mut Ctx<'_>,
    f: &Field<'a, 'i>,
    expected: &str,
) -> Option<Vec<Field<'a, 'i>>> {
    let array = as_array(ctx, f, expected)?;
    Some(
        array
            .iter()
            .enumerate()
            .map(|(i, value)| ctx.field(format!("{}[{i}]", f.path), value))
            .collect(),
    )
}

/// An array of strings, each accepted by `check`. `None` if any is refused.
fn string_list(
    ctx: &mut Ctx<'_>,
    f: &Field<'_, '_>,
    check: impl Fn(&str) -> Result<(), String>,
) -> Option<Vec<String>> {
    let items = elements(ctx, f, "an array of strings")?;
    let mut out = Vec::with_capacity(items.len());
    let mut ok = true;
    for item in &items {
        let accepted = as_str(ctx, item).and_then(|s| {
            let result = check(s).map(|()| s.to_string());
            checked(ctx, item, result)
        });
        match accepted {
            Some(s) => out.push(s),
            None => ok = false,
        }
    }
    ok.then_some(out)
}

/// A table of string values whose keys are accepted by `check_key`.
fn string_map(
    ctx: &mut Ctx<'_>,
    f: &Field<'_, '_>,
    value_hint: &str,
    check_key: impl Fn(&str) -> Result<(), String>,
    check_value: impl Fn(&str) -> Result<(), String>,
) -> Option<BTreeMap<String, String>> {
    let table = as_table(ctx, f)?;
    let mut out = BTreeMap::new();
    let mut ok = true;
    for (key, key_span, item) in table.into_fields(ctx) {
        if let Err(message) = check_key(key) {
            ctx.error_at(&item.path, &key_span, message);
            ok = false;
        }
        let value = match item.value.get_ref() {
            DeValue::String(s) => {
                let result = check_value(s).map(|()| s.to_string());
                checked(ctx, &item, result)
            }
            other => {
                let found = describe(other);
                ctx.error(
                    &item.path,
                    format!("expected a string, found {found}{value_hint}"),
                );
                None
            }
        };
        match value {
            Some(value) => {
                out.insert(key.to_string(), value);
            }
            None => ok = false,
        }
    }
    ok.then_some(out)
}

/// One of a fixed set of words. `_` and `-` are interchangeable.
fn as_choice<T: Copy>(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, choices: &[(&str, T)]) -> Option<T> {
    let s = as_str(ctx, f)?;
    let normalized = s.replace('_', "-");
    if let Some((_, value)) = choices.iter().find(|(name, _)| *name == normalized) {
        return Some(*value);
    }
    let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
    let hint = match closest(&normalized, &names) {
        Some(name) => format!("; did you mean `{name}`?"),
        None => String::new(),
    };
    ctx.error(
        &f.path,
        format!("unknown value `{s}`: use {}{hint}", text::one_of(&names)),
    );
    None
}

fn as_duration_secs(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<u64> {
    let DeValue::String(s) = f.value.get_ref() else {
        type_error(ctx, f, "a duration in quotes, such as \"30m\"");
        return None;
    };
    let secs = checked(ctx, f, units::parse_duration_secs(s))?;
    if secs == 0 {
        ctx.error(&f.path, format!("`{s}` must be longer than zero"));
        return None;
    }
    Some(secs)
}

fn as_bytes(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<u64> {
    let DeValue::String(s) = f.value.get_ref() else {
        type_error(ctx, f, "a size in quotes, such as \"8GiB\"");
        return None;
    };
    checked(ctx, f, units::parse_bytes(s))
}

fn as_cpu(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<u64> {
    let literal = match f.value.get_ref() {
        DeValue::Integer(integer) if integer.radix() == 10 => integer.as_str(),
        DeValue::Float(float) => float.as_str(),
        _ => {
            type_error(ctx, f, "a number of CPUs, such as 4 or 0.5");
            return None;
        }
    };
    let result = match literal.parse::<f64>() {
        Ok(cpus) => units::cpu_milli(cpus),
        Err(_) => Err(format!("`{literal}` is not a CPU count")),
    };
    checked(ctx, f, result)
}

// --------------------------------------------------------------- sections

type Sections = (
    Option<ProjectSection>,
    CaptureSection,
    BTreeMap<String, Recipe>,
);

fn document_sections(ctx: &mut Ctx<'_>, root: &DeTable<'_>) -> Sections {
    let mut root = Table::new("", root);

    let project = match root.take(ctx, "project") {
        Some(f) => project_section(ctx, &f),
        None => {
            ctx.push(
                Severity::Error,
                "project",
                None,
                "missing `[project]` section; it must give the project `name`".to_string(),
            );
            None
        }
    };

    let capture = root
        .take(ctx, "capture")
        .and_then(|f| capture_section(ctx, &f))
        .unwrap_or_default();

    let defaults = root
        .take(ctx, "defaults")
        .and_then(|f| {
            let mut table = as_table(ctx, &f)?;
            let settings = settings(ctx, &mut table);
            table.finish(ctx);
            Some(settings)
        })
        .unwrap_or_default();

    let mut recipes = BTreeMap::new();
    if let Some(f) = root.take(ctx, "recipes")
        && let Some(table) = as_table(ctx, &f)
    {
        for (name, name_span, recipe_field) in table.into_fields(ctx) {
            if let Err(message) = text::check_name(name, "recipe") {
                ctx.error_at(&recipe_field.path, &name_span, message);
            }
            if let Some(recipe) = recipe(ctx, name, &recipe_field, &defaults) {
                recipes.insert(name.to_string(), recipe);
            }
        }
    }

    root.finish(ctx);
    (project, capture, recipes)
}

fn project_section(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<ProjectSection> {
    let mut table = as_table(ctx, f)?;
    let name = match table.take(ctx, "name") {
        Some(name) => as_str(ctx, &name).and_then(|s| {
            let result = text::check_project_name(s).map(|()| s.to_string());
            checked(ctx, &name, result)
        }),
        None => {
            ctx.error(&f.path, "missing `name`: the project's name on the fleet");
            None
        }
    };
    table.finish(ctx);
    Some(ProjectSection { name: name? })
}

fn capture_section(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<CaptureSection> {
    let mut table = as_table(ctx, f)?;
    let mut patterns = |ctx: &mut Ctx<'_>, key: &'static str| {
        table
            .take(ctx, key)
            .and_then(|f| string_list(ctx, &f, text::check_pattern))
            .unwrap_or_default()
    };
    let exclude = patterns(ctx, "exclude");
    let include_ignored = patterns(ctx, "include_ignored");
    let secrets = patterns(ctx, "secrets");
    let allow_secrets = patterns(ctx, "allow_secrets");
    let roots = table
        .take(ctx, "roots")
        .and_then(|f| string_map(ctx, &f, "", text::check_mount, text::check_root_path))
        .unwrap_or_default();
    table.finish(ctx);
    Some(CaptureSection {
        exclude,
        include_ignored,
        secrets,
        allow_secrets,
        roots,
    })
}

// --------------------------------------------------------------- settings

/// The keys shared by `[defaults]` and every recipe, exactly as written.
#[derive(Clone, Default)]
struct Settings {
    hosts: Option<Vec<PlatformSelector>>,
    target: Option<String>,
    test_runtime: Option<PlatformSelector>,
    requires: Option<Vec<CapabilityRequirement>>,
    isolation: Option<IsolationLevel>,
    cpu: Option<u64>,
    memory: Option<u64>,
    disk: Option<u64>,
    max_cpu: Option<u64>,
    max_memory: Option<u64>,
    max_disk: Option<u64>,
    tokens: Option<BTreeMap<String, u64>>,
    parallelism: Option<u32>,
    priority: Option<Priority>,
    retry: Option<RetryClass>,
    max_attempts: Option<u32>,
    timeout: Option<u64>,
    cleanup_timeout: Option<u64>,
    disconnected: Option<u64>,
    cloud: Option<CloudMode>,
    lan_only: Option<bool>,
    artifacts: Option<Vec<String>>,
    test_reports: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    shell: Option<Vec<String>>,
    cwd: Option<String>,
}

impl Settings {
    /// Fill every key this recipe did not set from `[defaults]`. `env` is
    /// merged variable by variable; every other key is replaced whole.
    fn with_defaults(mut self, defaults: &Settings) -> Settings {
        macro_rules! inherit {
            ($($field:ident),* $(,)?) => {
                $(if self.$field.is_none() {
                    self.$field = defaults.$field.clone();
                })*
            };
        }
        inherit!(
            hosts,
            target,
            test_runtime,
            requires,
            isolation,
            cpu,
            memory,
            disk,
            max_cpu,
            max_memory,
            max_disk,
            tokens,
            parallelism,
            priority,
            retry,
            max_attempts,
            timeout,
            cleanup_timeout,
            disconnected,
            cloud,
            lan_only,
            artifacts,
            test_reports,
            shell,
            cwd,
        );
        if let Some(inherited) = &defaults.env {
            let mut env = inherited.clone();
            env.extend(self.env.take().unwrap_or_default());
            self.env = Some(env);
        }
        self
    }
}

const PRIORITIES: [(&str, Priority); 5] = [
    ("maintenance", Priority::Maintenance),
    ("background", Priority::Background),
    ("normal", Priority::Normal),
    ("interactive", Priority::Interactive),
    ("urgent", Priority::Urgent),
];

const RETRY_CLASSES: [(&str, RetryClass); 3] = [
    ("pure", RetryClass::Pure),
    ("isolated", RetryClass::IsolatedSideEffects),
    ("external", RetryClass::ExternalSideEffects),
];

const CLOUD_MODES: [(&str, CloudMode); 4] = [
    ("existing-only", CloudMode::ExistingOnly),
    ("manual", CloudMode::Manual),
    ("auto-overflow", CloudMode::AutoOverflow),
    ("hybrid", CloudMode::Hybrid),
];

const ISOLATION_LEVELS: [(&str, IsolationLevel); 3] = [
    ("native", IsolationLevel::Native),
    ("container", IsolationLevel::Container),
    ("vm", IsolationLevel::Vm),
];

const STEP_KINDS: [(&str, StepKind); 4] = [
    ("task", StepKind::Task),
    ("service", StepKind::Service),
    ("test", StepKind::Test),
    ("cleanup", StepKind::Cleanup),
];

fn settings(ctx: &mut Ctx<'_>, table: &mut Table<'_, '_>) -> Settings {
    macro_rules! key {
        ($name:literal, $parse:expr) => {
            table.take(ctx, $name).and_then(|f| $parse(ctx, &f))
        };
    }
    Settings {
        hosts: key!("hosts", hosts),
        target: key!("target", as_text),
        test_runtime: key!("test_runtime", selector),
        requires: key!("requires", requirements),
        isolation: key!("isolation", |ctx, f| as_choice(ctx, f, &ISOLATION_LEVELS)),
        cpu: key!("cpu", as_cpu),
        memory: key!("memory", as_bytes),
        disk: key!("disk", as_bytes),
        max_cpu: key!("max_cpu", as_cpu),
        max_memory: key!("max_memory", as_bytes),
        max_disk: key!("max_disk", as_bytes),
        tokens: key!("tokens", tokens),
        parallelism: key!("parallelism", |ctx, f| as_u32(
            ctx,
            f,
            "a number of parallel build jobs",
            1
        )),
        priority: key!("priority", |ctx, f| as_choice(ctx, f, &PRIORITIES)),
        retry: key!("retry", |ctx, f| as_choice(ctx, f, &RETRY_CLASSES)),
        max_attempts: key!("max_attempts", |ctx, f| as_u32(
            ctx,
            f,
            "a number of attempts",
            1
        )),
        timeout: key!("timeout", as_duration_secs),
        cleanup_timeout: key!("cleanup_timeout", as_duration_secs),
        disconnected: key!("disconnected", disconnected),
        cloud: key!("cloud", |ctx, f| as_choice(ctx, f, &CLOUD_MODES)),
        lan_only: key!("lan_only", as_bool),
        artifacts: key!("artifacts", |ctx, f| string_list(ctx, f, text::check_glob)),
        test_reports: key!("test_reports", |ctx, f| string_list(
            ctx,
            f,
            text::check_glob
        )),
        env: key!("env", env),
        shell: key!("shell", shell),
        cwd: key!("cwd", cwd).flatten(),
    }
}

/// `disconnected` may be zero: the attempt then stops as soon as its
/// authority window lapses without the controller.
fn disconnected(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<u64> {
    let DeValue::String(s) = f.value.get_ref() else {
        type_error(ctx, f, "a duration in quotes, such as \"10m\"");
        return None;
    };
    checked(ctx, f, units::parse_duration_secs(s))
}

fn hosts(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<Vec<PlatformSelector>> {
    let items = elements(ctx, f, "an array of platforms")?;
    if items.is_empty() {
        ctx.error(
            &f.path,
            "`hosts` is empty: list at least one platform, or remove the key to pin the job \
             to the platform that submits it",
        );
        return None;
    }
    let selectors: Vec<Option<PlatformSelector>> =
        items.iter().map(|item| selector(ctx, item)).collect();
    selectors.into_iter().collect()
}

fn selector(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<PlatformSelector> {
    match f.value.get_ref() {
        DeValue::String(s) => checked(ctx, f, text::selector_from_str(s)),
        DeValue::Table(_) => {
            let mut table = as_table(ctx, f)?;
            let errors = ctx.error_count();
            let os = table.take(ctx, "os").and_then(|f| {
                let s = as_str(ctx, &f)?;
                checked(ctx, &f, text::os_from_str(s))
            });
            let arch = table.take(ctx, "arch").and_then(|f| {
                let s = as_str(ctx, &f)?;
                checked(ctx, &f, text::arch_from_str(s))
            });
            let os_version = table.take(ctx, "os_version").and_then(|f| {
                let s = as_str(ctx, &f)?;
                checked(ctx, &f, text::version_requirement(s))
            });
            table.finish(ctx);
            (ctx.error_count() == errors).then_some(PlatformSelector {
                os: os.flatten(),
                arch: arch.flatten(),
                os_version,
            })
        }
        _ => {
            type_error(
                ctx,
                f,
                "a platform such as \"linux\" or \"macos-aarch64\", or a table with `os`, `arch` \
                 and `os_version`",
            );
            None
        }
    }
}

fn requirements(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<Vec<CapabilityRequirement>> {
    let items = elements(ctx, f, "an array of requirements")?;
    let parsed: Vec<Option<CapabilityRequirement>> =
        items.iter().map(|item| requirement(ctx, item)).collect();
    parsed.into_iter().collect()
}

fn requirement(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<CapabilityRequirement> {
    match f.value.get_ref() {
        DeValue::String(s) => checked(ctx, f, text::requirement_from_str(s)),
        DeValue::Table(_) => {
            let mut table = as_table(ctx, f)?;
            let errors = ctx.error_count();
            let name = match table.take(ctx, "name") {
                Some(name) => as_str(ctx, &name).and_then(|s| {
                    let result = text::check_capability_name(s).map(|()| s.to_string());
                    checked(ctx, &name, result)
                }),
                None => {
                    ctx.error(&f.path, "missing `name`: the capability that is required");
                    None
                }
            };
            let version = table.take(ctx, "version").and_then(|f| {
                let s = as_str(ctx, &f)?;
                checked(ctx, &f, text::version_requirement(s))
            });
            let attributes = table
                .take(ctx, "attributes")
                .and_then(|f| string_map(ctx, &f, "", text::check_attribute, |_| Ok(())))
                .unwrap_or_default();
            table.finish(ctx);
            let name = name?;
            (ctx.error_count() == errors).then_some(CapabilityRequirement {
                name,
                version,
                attributes,
            })
        }
        _ => {
            type_error(
                ctx,
                f,
                "a requirement such as \"rust>=1.85\", or a table with `name`, `version` and \
                 `attributes`",
            );
            None
        }
    }
}

/// Named countable resources. Zero counts are dropped, so `gpu = 0` and no
/// `gpu` entry are the same declaration.
fn tokens(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<BTreeMap<String, u64>> {
    let table = as_table(ctx, f)?;
    let mut out = BTreeMap::new();
    let mut ok = true;
    for (name, name_span, item) in table.into_fields(ctx) {
        if let Err(message) = text::check_token_name(name) {
            ctx.error_at(&item.path, &name_span, message);
            ok = false;
        }
        match as_int(ctx, &item, "a count", 0, u64::MAX) {
            Some(0) => {}
            Some(count) => {
                out.insert(name.to_string(), count);
            }
            None => ok = false,
        }
    }
    ok.then_some(out)
}

fn env(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<BTreeMap<String, String>> {
    string_map(
        ctx,
        f,
        "; environment values are strings, so quote it",
        text::check_env_name,
        text::check_env_value,
    )
}

fn shell(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<Vec<String>> {
    let argv = string_list(ctx, f, |_| Ok(()))?;
    if argv.first().is_none_or(|program| program.is_empty()) {
        ctx.error(
            &f.path,
            "`shell` needs a program, such as [\"bash\", \"-c\"]; the script is added as the \
             last argument",
        );
        return None;
    }
    Some(argv)
}

/// `Some(None)` is the source root itself, written `.`.
fn cwd(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<Option<String>> {
    let s = as_str(ctx, f)?;
    checked(ctx, f, text::working_directory(s))
}

fn command(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, shell: Option<&[String]>) -> Option<CommandSpec> {
    match f.value.get_ref() {
        DeValue::String(script) => {
            if script.trim().is_empty() {
                ctx.error(&f.path, "the command is empty");
                return None;
            }
            Some(CommandSpec::Shell {
                script: script.to_string(),
                shell: shell.map(<[String]>::to_vec),
            })
        }
        DeValue::Array(_) => {
            let argv = string_list(ctx, f, |_| Ok(()))?;
            if argv.first().is_none_or(|program| program.is_empty()) {
                ctx.error(
                    &f.path,
                    "the command is empty: give the program followed by its arguments",
                );
                return None;
            }
            Some(CommandSpec::Argv { argv })
        }
        _ => {
            type_error(
                ctx,
                f,
                "a command: a string (run by a shell) or an array (a program and its arguments)",
            );
            None
        }
    }
}

// ----------------------------------------------------------------- recipes

/// A step as written, before the graph is checked.
struct RawStep {
    path: String,
    /// `None` when the name was missing or not a string.
    name: Option<String>,
    kind: StepKind,
    command: Option<CommandSpec>,
    cwd: Option<String>,
    env: BTreeMap<String, String>,
    /// Each entry of `needs` with the path it was written at.
    needs: Vec<(String, String)>,
    /// Path of the `ready` key, when present, and the probe if it was valid.
    ready: Option<(String, Option<Readiness>)>,
    timeout: Option<u64>,
    resources: Option<ResourceVector>,
    /// Each port with the path it was written at.
    ports: Vec<(u16, String)>,
}

fn recipe(ctx: &mut Ctx<'_>, name: &str, f: &Field<'_, '_>, defaults: &Settings) -> Option<Recipe> {
    let first_issue = ctx.issues.len();
    let errors = ctx.error_count();
    let mut table = as_table(ctx, f)?;

    let description = table
        .take(ctx, "description")
        .and_then(|f| as_str(ctx, &f).map(str::to_string));
    let own = settings(ctx, &mut table);
    let run = table.take(ctx, "run");
    let steps_field = table.take(ctx, "steps");
    let environment_field = table.take(ctx, "environment");
    table.finish(ctx);

    let merged = own.clone().with_defaults(defaults);
    // Where a merged key was written, for messages about its value.
    let origin = |own_has: bool, key: &str| {
        if own_has {
            format!("{}.{key}", f.path)
        } else {
            format!("defaults.{key}")
        }
    };

    let raw_steps: Vec<RawStep> = match (&run, &steps_field) {
        (Some(run), Some(steps_field)) => {
            ctx.error(
                &run.path,
                "`run` and `steps` are mutually exclusive: `run` is shorthand for one step \
                 named `main`",
            );
            // The steps are still checked, so their problems show up now
            // rather than after `run` is removed.
            steps(ctx, steps_field, &merged)
        }
        (Some(run), None) => {
            let command = command(ctx, run, merged.shell.as_deref());
            vec![RawStep {
                path: f.path.clone(),
                name: Some(MAIN_STEP.to_string()),
                kind: StepKind::Task,
                command,
                cwd: merged.cwd.clone(),
                env: BTreeMap::new(),
                needs: Vec::new(),
                ready: None,
                timeout: None,
                resources: None,
                ports: Vec::new(),
            }]
        }
        (None, Some(steps_field)) => steps(ctx, steps_field, &merged),
        (None, None) => {
            ctx.error(
                &f.path,
                format!(
                    "recipe has nothing to run: add `run = \"...\"` or `[[recipes.{name}.steps]]`"
                ),
            );
            Vec::new()
        }
    };

    step_rules(ctx, &raw_steps);
    let order = graph::order(&raw_steps_for_graph(&raw_steps)).unwrap_or_else(|problems| {
        for problem in problems {
            ctx.error(&problem.path, problem.message);
        }
        Vec::new()
    });

    let has_service = raw_steps.iter().any(|s| s.kind == StepKind::Service);
    let environment = environment_field.and_then(|field| {
        let lease = environment(ctx, &field);
        if !has_service {
            ctx.error(
                &field.path,
                "`environment` keeps services alive, but this recipe has no step with \
                 `kind = \"service\"`",
            );
        }
        lease
    });
    if has_service && environment.is_none() && raw_steps.iter().all(|s| s.kind == StepKind::Service)
    {
        ctx.warn(
            &f.path,
            "every step is a service and there is no `[environment]`, so the services are \
             stopped as soon as they are ready",
        );
    }

    if let (Some(hosts), Some(runtime)) = (&merged.hosts, &merged.test_runtime)
        && !hosts
            .iter()
            .any(|host| text::selectors_overlap(host, runtime))
    {
        ctx.warn(
            &origin(own.test_runtime.is_some(), "test_runtime"),
            format!(
                "no build host in `hosts` can also be the test runtime `{runtime}`; a job runs \
                 on one machine, so this recipe cannot be placed"
            ),
        );
    }

    // Everything below needs a recipe that is otherwise sound.
    let envelope = if ctx.error_count() == errors {
        envelope(ctx, &raw_steps, &merged, &own, &origin, &f.path)
    } else {
        None
    };
    if ctx.error_count() != errors {
        return None;
    }
    let envelope = envelope?;

    let mut by_index: Vec<Option<RawStep>> = raw_steps.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(by_index.len());
    for index in order {
        let raw = by_index.get_mut(index).and_then(Option::take)?;
        ordered.push(StepSpec {
            name: raw.name?,
            kind: raw.kind,
            command: raw.command?,
            cwd: raw.cwd,
            env: raw.env,
            depends_on: raw.needs.into_iter().map(|(name, _)| name).collect(),
            readiness: raw.ready.and_then(|(_, readiness)| readiness),
            timeout_secs: raw.timeout,
            resources: raw.resources,
            ports: raw.ports.into_iter().map(|(port, _)| port).collect(),
        });
    }

    let mut warnings: Vec<Issue> = ctx.issues[first_issue..]
        .iter()
        .filter(|issue| !issue.is_error())
        .cloned()
        .collect();
    sort_issues(&mut warnings);

    Some(Recipe {
        name: name.to_string(),
        description,
        steps: ordered,
        envelope,
        compatibility: Compatibility {
            build_hosts: merged.hosts.unwrap_or_default(),
            artifact_target: merged.target,
            test_runtime: merged.test_runtime,
            requires: merged.requires.unwrap_or_default(),
            isolation: merged.isolation.unwrap_or_default(),
        },
        priority: merged.priority.unwrap_or_default(),
        retry_class: merged.retry.unwrap_or_default(),
        max_attempts: merged.max_attempts,
        timeout_secs: merged.timeout,
        cleanup_timeout_secs: merged.cleanup_timeout,
        cloud: merged.cloud.unwrap_or_default(),
        lan_only: merged.lan_only.unwrap_or(false),
        disconnected_secs: merged.disconnected,
        artifacts: merged.artifacts.unwrap_or_default(),
        test_reports: merged.test_reports.unwrap_or_default(),
        env: merged.env.unwrap_or_default(),
        environment,
        parallelism: merged.parallelism,
        warnings,
    })
}

fn steps(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, recipe: &Settings) -> Vec<RawStep> {
    let Some(items) = elements(ctx, f, "an array of step tables") else {
        return Vec::new();
    };
    if items.is_empty() {
        ctx.error(
            &f.path,
            "`steps` is empty: a recipe needs at least one step",
        );
    }
    if items.len() > MAX_STEPS {
        ctx.error(
            &f.path,
            format!(
                "{} steps is more than the {MAX_STEPS} a recipe may have",
                items.len()
            ),
        );
        return Vec::new();
    }
    items
        .iter()
        .filter_map(|item| step(ctx, item, recipe))
        .collect()
}

fn step(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, recipe: &Settings) -> Option<RawStep> {
    let mut table = as_table(ctx, f)?;

    let name = match table.take(ctx, "name") {
        Some(name) => as_str(ctx, &name).map(|s| {
            if let Err(message) = text::check_name(s, "step") {
                ctx.error(&name.path, message);
            }
            s.to_string()
        }),
        None => {
            ctx.error(&f.path, "missing `name`: every step needs a unique name");
            None
        }
    };
    let kind = table
        .take(ctx, "kind")
        .and_then(|f| as_choice(ctx, &f, &STEP_KINDS))
        .unwrap_or_default();

    let own_shell = table.take(ctx, "shell");
    let shell_argv = match &own_shell {
        Some(f) => shell(ctx, f),
        None => recipe.shell.clone(),
    };
    let command = match table.take(ctx, "run") {
        Some(run) => {
            if let (Some(own_shell), DeValue::Array(_)) = (&own_shell, run.value.get_ref()) {
                ctx.error(
                    &own_shell.path,
                    "`shell` only applies when `run` is a string; an array is run without a shell",
                );
            }
            command(ctx, &run, shell_argv.as_deref())
        }
        None => {
            ctx.error(&f.path, "missing `run`: the command this step executes");
            None
        }
    };

    let cwd = match table.take(ctx, "cwd") {
        Some(f) => cwd(ctx, &f).flatten(),
        None => recipe.cwd.clone(),
    };
    let env = table
        .take(ctx, "env")
        .and_then(|f| env(ctx, &f))
        .unwrap_or_default();

    let mut needs = Vec::new();
    if let Some(f) = table.take(ctx, "needs")
        && let Some(items) = elements(ctx, &f, "an array of step names")
    {
        for item in &items {
            if let Some(name) = as_str(ctx, item) {
                needs.push((name.to_string(), item.path.clone()));
            }
        }
    }

    let ready = table
        .take(ctx, "ready")
        .map(|f| (f.path.clone(), readiness(ctx, &f, shell_argv.as_deref())));
    let timeout = table
        .take(ctx, "timeout")
        .and_then(|f| as_duration_secs(ctx, &f));

    let cpu = table.take(ctx, "cpu").and_then(|f| as_cpu(ctx, &f));
    let memory = table.take(ctx, "memory").and_then(|f| as_bytes(ctx, &f));
    let disk = table.take(ctx, "disk").and_then(|f| as_bytes(ctx, &f));
    let step_tokens = table.take(ctx, "tokens").and_then(|f| tokens(ctx, &f));
    let declares_resources =
        cpu.is_some() || memory.is_some() || disk.is_some() || step_tokens.is_some();
    let resources = declares_resources.then(|| ResourceVector {
        cpu_milli: cpu.unwrap_or(0),
        memory_bytes: memory.unwrap_or(0),
        disk_bytes: disk.unwrap_or(0),
        tokens: step_tokens.unwrap_or_default(),
    });

    let mut ports = Vec::new();
    if let Some(f) = table.take(ctx, "ports")
        && let Some(items) = elements(ctx, &f, "an array of port numbers")
    {
        for item in &items {
            if let Some(port) = as_port(ctx, item) {
                ports.push((port, item.path.clone()));
            }
        }
    }

    table.finish(ctx);
    Some(RawStep {
        path: f.path.clone(),
        name,
        kind,
        command,
        cwd,
        env,
        needs,
        ready,
        timeout,
        resources,
        ports,
    })
}

fn as_port(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<u16> {
    let port = as_int(ctx, f, "a port number", 1, u64::from(u16::MAX))?;
    u16::try_from(port).ok()
}

fn readiness(ctx: &mut Ctx<'_>, f: &Field<'_, '_>, shell: Option<&[String]>) -> Option<Readiness> {
    let mut table = as_table(ctx, f)?;
    let errors = ctx.error_count();

    let tcp = table.take(ctx, "tcp");
    let http = table.take(ctx, "http");
    let log = table.take(ctx, "log");
    let run = table.take(ctx, "run");
    let path = table.take(ctx, "path");
    let status = table.take(ctx, "status");
    let timeout = table.take(ctx, "timeout");
    let interval = table.take(ctx, "interval");
    table.finish(ctx);

    let probes = [&tcp, &http, &log, &run];
    if probes.iter().filter(|probe| probe.is_some()).count() != 1 {
        ctx.error(
            &f.path,
            "`ready` needs exactly one probe: `tcp`, `http`, `log` or `run`",
        );
        return None;
    }
    if http.is_none() {
        for extra in [&path, &status].into_iter().flatten() {
            ctx.error(&extra.path, "only applies to an `http` probe");
        }
    }

    let probe = if let Some(tcp) = &tcp {
        as_port(ctx, tcp).map(|port| ReadinessProbe::Tcp { port })
    } else if let Some(http) = &http {
        let port = as_port(ctx, http);
        let path = match &path {
            Some(path) => as_str(ctx, path).and_then(|s| {
                let result = text::check_http_path(s).map(|()| s.to_string());
                checked(ctx, path, result)
            }),
            None => Some("/".to_string()),
        };
        let status = match &status {
            Some(status) => as_int(ctx, status, "an HTTP status", 100, 599)
                .and_then(|code| u16::try_from(code).ok())
                .map(Some),
            None => Some(None),
        };
        match (port, path, status) {
            (Some(port), Some(path), Some(status)) => {
                Some(ReadinessProbe::Http { port, path, status })
            }
            _ => None,
        }
    } else if let Some(log) = &log {
        as_text(ctx, log).map(|contains| ReadinessProbe::LogLine { contains })
    } else if let Some(run) = &run {
        command(ctx, run, shell).map(|command| ReadinessProbe::Command { command })
    } else {
        None
    };

    let timeout_secs = match &timeout {
        Some(timeout) => as_duration_secs(ctx, timeout),
        None => Some(DEFAULT_READY_TIMEOUT_SECS),
    };
    let interval_ms = match &interval {
        Some(interval) => as_str(ctx, interval).and_then(|s| {
            let result = units::parse_duration_ms(s).and_then(|ms| {
                if ms == 0 {
                    Err(format!("`{s}` must be longer than zero"))
                } else {
                    Ok(ms)
                }
            });
            checked(ctx, interval, result)
        }),
        None => Some(DEFAULT_READY_INTERVAL_MS),
    };

    if ctx.error_count() != errors {
        return None;
    }
    Some(Readiness {
        probe: probe?,
        timeout_secs: timeout_secs?,
        interval_ms: interval_ms?,
    })
}

fn environment(ctx: &mut Ctx<'_>, f: &Field<'_, '_>) -> Option<LeaseSpec> {
    let mut table = as_table(ctx, f)?;
    let errors = ctx.error_count();
    let defaults = LeaseSpec::default();
    let idle = table.take(ctx, "idle_timeout");
    let lease = table.take(ctx, "lease");
    let max_lifetime = table.take(ctx, "max_lifetime");
    table.finish(ctx);

    let read = |ctx: &mut Ctx<'_>, field: &Option<Field<'_, '_>>, default: u64| match field {
        Some(field) => as_duration_secs(ctx, field),
        None => Some(default),
    };
    let idle_timeout_secs = read(ctx, &idle, defaults.idle_timeout_secs);
    let lease_secs = read(ctx, &lease, defaults.lease_secs);
    let max_lifetime_secs = read(ctx, &max_lifetime, defaults.max_lifetime_secs);
    if ctx.error_count() != errors {
        return None;
    }
    let spec = LeaseSpec {
        idle_timeout_secs: idle_timeout_secs?,
        lease_secs: lease_secs?,
        max_lifetime_secs: max_lifetime_secs?,
    };
    if spec.lease_secs > spec.max_lifetime_secs {
        let at = lease
            .as_ref()
            .or(max_lifetime.as_ref())
            .map_or(f.path.as_str(), |field| field.path.as_str());
        ctx.error(
            at,
            format!(
                "`lease` of {} is longer than `max_lifetime` of {}",
                units::format_duration_secs(spec.lease_secs),
                units::format_duration_secs(spec.max_lifetime_secs)
            ),
        );
        return None;
    }
    Some(spec)
}

/// Rules about single steps and about ports, which need no graph.
fn step_rules(ctx: &mut Ctx<'_>, steps: &[RawStep]) {
    let mut port_owner: BTreeMap<u16, String> = BTreeMap::new();
    for step in steps {
        let label = step.name.as_deref().unwrap_or("?");
        let is_service = step.kind == StepKind::Service;
        match &step.ready {
            Some((path, _)) if !is_service => ctx.error(
                path,
                format!(
                    "`ready` only applies to a step with `kind = \"service\"`; `{label}` runs to \
                     completion"
                ),
            ),
            None if is_service => ctx.warn(
                &step.path,
                format!(
                    "service `{label}` has no `ready` probe, so steps that need it start as \
                     soon as its process does"
                ),
            ),
            _ => {}
        }
        for (port, path) in &step.ports {
            if !is_service {
                ctx.error(
                    path,
                    format!("`ports` only applies to a step with `kind = \"service\"`; `{label}` is not one"),
                );
            } else if let Some(owner) = port_owner.get(port) {
                let owner = if owner == label {
                    "this step".to_string()
                } else {
                    format!("step `{owner}`")
                };
                ctx.error(
                    path,
                    format!("port {port} is already used by {owner}; a job runs on one machine"),
                );
            } else {
                port_owner.insert(*port, label.to_string());
            }
        }
    }
}

fn raw_steps_for_graph(steps: &[RawStep]) -> Vec<graph::Node<'_>> {
    steps
        .iter()
        .map(|step| graph::Node {
            path: &step.path,
            name: step.name.as_deref(),
            cleanup: step.kind == StepKind::Cleanup,
            needs: &step.needs,
        })
        .collect()
}

/// Peak simultaneous demand of a recipe.
///
/// Services run together and alongside everything else, so their resources
/// add up. The other steps never overlap in v1, so they contribute their
/// component-wise maximum. The recipe-level declaration is a lower bound.
fn envelope(
    ctx: &mut Ctx<'_>,
    steps: &[RawStep],
    merged: &Settings,
    own: &Settings,
    origin: &dyn Fn(bool, &str) -> String,
    recipe_path: &str,
) -> Option<ResourceEnvelope> {
    let mut services = ResourceVector::ZERO;
    let mut others = ResourceVector::ZERO;
    for step in steps {
        let Some(resources) = &step.resources else {
            continue;
        };
        if step.kind == StepKind::Service {
            services = services.checked_add(resources).or_else(|| {
                ctx.error(
                    &step.path,
                    "the services' resources add up to more than can be counted",
                );
                None
            })?;
        } else {
            others = others.max(resources);
        }
    }
    let Some(needed) = services.checked_add(&others) else {
        ctx.error(
            recipe_path,
            "the steps' resources add up to more than can be counted",
        );
        return None;
    };

    let declared = ResourceVector {
        cpu_milli: merged.cpu.unwrap_or(0),
        memory_bytes: merged.memory.unwrap_or(0),
        disk_bytes: merged.disk.unwrap_or(0),
        tokens: merged.tokens.clone().unwrap_or_default(),
    };
    let peak = needed.max(&declared);

    let mut understated = |own_has: bool, key: &str, needed: String, declared: String| {
        ctx.warn(
            &origin(own_has, key),
            format!(
                "the steps need {needed} at their peak, more than the {declared} declared here; \
                 the job reserves {needed}"
            ),
        );
    };
    if let Some(cpu) = merged.cpu
        && needed.cpu_milli > cpu
    {
        understated(
            own.cpu.is_some(),
            "cpu",
            units::format_cpus(needed.cpu_milli),
            units::format_cpus(cpu),
        );
    }
    if let Some(memory) = merged.memory
        && needed.memory_bytes > memory
    {
        understated(
            own.memory.is_some(),
            "memory",
            format!("{} of memory", units::format_bytes(needed.memory_bytes)),
            units::format_bytes(memory),
        );
    }
    if let Some(disk) = merged.disk
        && needed.disk_bytes > disk
    {
        understated(
            own.disk.is_some(),
            "disk",
            format!("{} of disk", units::format_bytes(needed.disk_bytes)),
            units::format_bytes(disk),
        );
    }
    if let Some(tokens) = &merged.tokens {
        for (name, count) in tokens {
            let needed = needed.token(name);
            if needed > *count {
                understated(
                    own.tokens.is_some(),
                    "tokens",
                    format!("{needed} `{name}`"),
                    count.to_string(),
                );
            }
        }
    }

    let mut maximum = peak.clone();
    let mut ok = true;
    let mut ceiling = |own_has: bool,
                       key: &str,
                       limit: Option<u64>,
                       peak: u64,
                       slot: &mut u64,
                       show: &dyn Fn(u64) -> String| {
        let Some(limit) = limit else { return };
        if limit < peak {
            ctx.error(
                &origin(own_has, key),
                format!(
                    "`{key}` of {} is below the {} the recipe needs at its peak",
                    show(limit),
                    show(peak)
                ),
            );
            ok = false;
        } else {
            *slot = limit;
        }
    };
    ceiling(
        own.max_cpu.is_some(),
        "max_cpu",
        merged.max_cpu,
        peak.cpu_milli,
        &mut maximum.cpu_milli,
        &units::format_cpus,
    );
    ceiling(
        own.max_memory.is_some(),
        "max_memory",
        merged.max_memory,
        peak.memory_bytes,
        &mut maximum.memory_bytes,
        &units::format_bytes,
    );
    ceiling(
        own.max_disk.is_some(),
        "max_disk",
        merged.max_disk,
        peak.disk_bytes,
        &mut maximum.disk_bytes,
        &units::format_bytes,
    );
    if !ok {
        return None;
    }

    let missing = match (peak.cpu_milli == 0, peak.memory_bytes == 0) {
        (true, true) => Some("`cpu` or `memory`"),
        (true, false) => Some("`cpu`"),
        (false, true) => Some("`memory`"),
        (false, false) => None,
    };
    if let Some(missing) = missing {
        ctx.warn(
            recipe_path,
            format!(
                "no {missing} is declared, so the job reserves none and may be placed on a \
                 machine without room for it"
            ),
        );
    }

    Some(ResourceEnvelope {
        initial: peak.clone(),
        peak,
        maximum,
    })
}

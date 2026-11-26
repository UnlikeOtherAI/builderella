//! The arguments that describe what to run, shared by `plan_job` and
//! `submit_job`.

use super::{Call, Failure, flag_arg, text_arg};
use crate::sanitize::inline;
use bella_core::job::{CommandSpec, Priority};
use bella_core::platform::{Arch, Os, PlacementChoice, PlatformSelector};
use bella_core::resources::ResourceVector;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

const MAX_ENV_NAME_CHARS: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 8 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;

/// What to run and where, after validation.
pub(crate) struct Work {
    pub workspace: PathBuf,
    pub recipe: Option<String>,
    pub command: Option<CommandSpec>,
    pub priority: Option<Priority>,
    pub placement: Option<PlacementChoice>,
    pub env: BTreeMap<String, String>,
    pub resources: Option<ResourceVector>,
}

/// Schema properties for [`parse`].
pub(crate) fn properties() -> Map<String, Value> {
    let properties = json!({
        "workspace": {
            "type": "string",
            "minLength": 1,
            "maxLength": 4096,
            "description": "Absolute path of the project directory. Must be inside a directory this server was started for; defaults to the first of them."
        },
        "recipe": {
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "description": "Name of a recipe defined in the workspace's builderella.toml. Give exactly one of `recipe` or `command`."
        },
        "command": {
            "type": ["string", "array"],
            "minLength": 1,
            "minItems": 1,
            "maxItems": 256,
            "items": {"type": "string"},
            "description": "Ad-hoc command instead of a recipe. A string is run as a script by the worker's default shell; an array is an argument vector run without a shell. Give exactly one of `recipe` or `command`."
        },
        "priority": {
            "type": "string",
            "enum": ["maintenance", "background", "normal", "interactive", "urgent"],
            "description": "Priority band. Defaults to the recipe's own, normally `normal`. Project policy may refuse `urgent`."
        },
        "platform": {
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "description": "Where to run: an operating system (`linux`, `macos`, `windows`), optionally with an architecture (`linux-x86_64`, `macos-aarch64`). A preference unless `require` is true."
        },
        "require": {
            "type": "boolean",
            "description": "With `platform`: true makes it a hard requirement that is never substituted; false or absent makes it a preference."
        },
        "machine": {
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "description": "Run only on this machine: its id or unique name from list_fleet. Cannot be combined with `platform`."
        },
        "env": {
            "type": "object",
            "maxProperties": 64,
            "additionalProperties": {"type": "string"},
            "description": "Extra environment variables for the job's steps. They are stored with the job, so do not pass secrets."
        },
        "cpu": {
            "type": "number",
            "minimum": 0.001,
            "maximum": 4096,
            "description": "CPUs to reserve, for example 4 or 0.5, instead of what the recipe asks for. Fleet policy may clamp or refuse it."
        },
        "memory": {
            "type": "string",
            "minLength": 1,
            "maxLength": 32,
            "description": "Memory to reserve, with a unit: `512MiB`, `8GiB`. Replaces what the recipe asks for. Fleet policy may clamp or refuse it."
        }
    });
    match properties {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Validate the arguments and confine the workspace to the allowed roots.
pub(crate) fn parse(call: &Call<'_>, arguments: &Map<String, Value>) -> Result<Work, Failure> {
    let recipe = text_arg(arguments, "recipe").map(str::trim);
    let command = arguments.get("command");
    let (recipe, command) = match (recipe, command) {
        (Some(_), Some(_)) => {
            return Err(Failure::Invalid(
                "give either `recipe` or `command`, not both".into(),
            ));
        }
        (None, None) => {
            return Err(Failure::Invalid(
                "give one of `recipe` (a name from builderella.toml) or `command`".into(),
            ));
        }
        (Some(""), None) => return Err(Failure::Invalid("`recipe` must not be blank".into())),
        (Some(name), None) => (Some(name.to_string()), None),
        (None, Some(value)) => (None, Some(command_spec(value)?)),
    };

    let placement = placement(arguments)?;
    let priority = match arguments.get("priority") {
        Some(value) => Some(
            serde_json::from_value::<Priority>(value.clone())
                .map_err(|_| Failure::Invalid("`priority` is not a known priority band".into()))?,
        ),
        None => None,
    };
    let env = environment(arguments)?;
    let resources = resources(arguments)?;

    // Last, so that a call with bad arguments never touches the filesystem.
    let roots = &call.config.roots;
    let workspace = match text_arg(arguments, "workspace") {
        Some(raw) => roots
            .existing_dir("workspace", raw)
            .map_err(Failure::Invalid)?,
        None => roots.first().ok_or_else(|| {
            Failure::Invalid(
                "no `workspace` was given and this server was started without a usable directory; ask the user to restart `bella mcp` with `--root <directory>`".into(),
            )
        })?,
    };

    Ok(Work {
        workspace,
        recipe,
        command,
        priority,
        placement,
        env,
        resources,
    })
}

fn command_spec(value: &Value) -> Result<CommandSpec, Failure> {
    match value {
        Value::String(script) => {
            if script.trim().is_empty() {
                return Err(Failure::Invalid("`command` must not be blank".into()));
            }
            check_command_text("command", script)?;
            Ok(CommandSpec::Shell {
                script: script.clone(),
                shell: None,
            })
        }
        Value::Array(items) => {
            let argv: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if argv.first().is_none_or(|program| program.trim().is_empty()) {
                return Err(Failure::Invalid(
                    "`command[0]` must name the program to run".into(),
                ));
            }
            let total: usize = argv.iter().map(String::len).sum();
            if total > MAX_COMMAND_BYTES {
                return Err(Failure::Invalid(format!(
                    "`command` is longer than {MAX_COMMAND_BYTES} bytes"
                )));
            }
            for (index, argument) in argv.iter().enumerate() {
                check_command_text(&format!("command[{index}]"), argument)?;
            }
            Ok(CommandSpec::Argv { argv })
        }
        _ => Err(Failure::Invalid(
            "`command` must be a string or an array of strings".into(),
        )),
    }
}

fn check_command_text(what: &str, text: &str) -> Result<(), Failure> {
    if text.contains('\0') {
        return Err(Failure::Invalid(format!(
            "`{what}` contains a NUL character"
        )));
    }
    if text.len() > MAX_COMMAND_BYTES {
        return Err(Failure::Invalid(format!(
            "`{what}` is longer than {MAX_COMMAND_BYTES} bytes"
        )));
    }
    Ok(())
}

fn placement(arguments: &Map<String, Value>) -> Result<Option<PlacementChoice>, Failure> {
    let platform = text_arg(arguments, "platform");
    let machine = text_arg(arguments, "machine");
    let require = flag_arg(arguments, "require");
    match (platform, machine) {
        (Some(_), Some(_)) => Err(Failure::Invalid(
            "give either `platform` or `machine`, not both".into(),
        )),
        (None, Some(machine)) => {
            if require.is_some() {
                return Err(Failure::Invalid(
                    "`require` applies to `platform` only; `machine` is always a hard choice"
                        .into(),
                ));
            }
            let machine = machine.trim();
            if machine.is_empty() || machine.chars().any(char::is_control) {
                return Err(Failure::Invalid(
                    "`machine` must be a machine id or name from list_fleet".into(),
                ));
            }
            Ok(Some(PlacementChoice::Machine {
                machine: machine.to_string(),
            }))
        }
        (Some(platform), None) => {
            let platform = platform_selector(platform).map_err(Failure::Invalid)?;
            Ok(Some(if require == Some(true) {
                PlacementChoice::Require { platform }
            } else {
                PlacementChoice::Prefer { platform }
            }))
        }
        (None, None) => {
            if require.is_some() {
                return Err(Failure::Invalid("`require` needs `platform`".into()));
            }
            Ok(None)
        }
    }
}

/// `linux`, `macos-aarch64`, `windows-x86_64`, with the aliases
/// `bella-core` accepts (`darwin`, `arm64`, `amd64`, `x64`).
pub(crate) fn platform_selector(text: &str) -> Result<PlatformSelector, String> {
    let lowered = text.trim().to_ascii_lowercase();
    let (os, arch) = match lowered.split_once('-') {
        Some((os, arch)) => (os, Some(arch)),
        None => (lowered.as_str(), None),
    };
    let hint = "expected an operating system (`linux`, `macos`, `windows`), optionally followed by `-x86_64` or `-aarch64`";
    let os: Os = os
        .parse()
        .map_err(|_| format!("`platform` `{}` is not known: {hint}", inline(text, 64)))?;
    let arch = match arch {
        Some(arch) => Some(
            arch.parse::<Arch>()
                .map_err(|_| format!("`platform` `{}` is not known: {hint}", inline(text, 64)))?,
        ),
        None => None,
    };
    Ok(PlatformSelector {
        os: Some(os),
        arch,
        os_version: None,
    })
}

fn environment(arguments: &Map<String, Value>) -> Result<BTreeMap<String, String>, Failure> {
    let mut env = BTreeMap::new();
    let Some(entries) = arguments.get("env").and_then(Value::as_object) else {
        return Ok(env);
    };
    for (name, value) in entries {
        let shown = inline(name, 64);
        if name.is_empty()
            || name.chars().count() > MAX_ENV_NAME_CHARS
            || name.contains('=')
            || name.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(Failure::Invalid(format!(
                "`env` name `{shown}` is not a valid environment variable name"
            )));
        }
        let value = value.as_str().unwrap_or_default();
        if value.contains('\0') {
            return Err(Failure::Invalid(format!(
                "`env.{shown}` contains a NUL character"
            )));
        }
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(Failure::Invalid(format!(
                "`env.{shown}` is longer than {MAX_ENV_VALUE_BYTES} bytes"
            )));
        }
        env.insert(name.clone(), value.to_string());
    }
    Ok(env)
}

fn resources(arguments: &Map<String, Value>) -> Result<Option<ResourceVector>, Failure> {
    let cpu = arguments.get("cpu").and_then(Value::as_f64);
    let memory = text_arg(arguments, "memory");
    if cpu.is_none() && memory.is_none() {
        return Ok(None);
    }
    let mut vector = ResourceVector::default();
    if let Some(cpu) = cpu {
        // The schema bounds the value; at least one milli-CPU remains.
        vector.cpu_milli = ((cpu * 1000.0).round() as u64).max(1);
    }
    if let Some(memory) = memory {
        vector.memory_bytes = memory_bytes(memory).map_err(Failure::Invalid)?;
    }
    Ok(Some(vector))
}

/// Parse `512MiB`, `8GiB`, `1.5G`, `2000000000` (bytes). `KiB`-style units
/// and bare letters are powers of 1024; `KB`-style units are powers of 1000.
pub(crate) fn memory_bytes(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let invalid = || {
        format!(
            "`memory` `{}` is not a size: write a number with a unit, such as `512MiB` or `8GiB`",
            inline(text, 32)
        )
    };
    let value: f64 = number.parse().map_err(|_| invalid())?;
    let factor: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kib" => 1024.0,
        "m" | "mib" => 1024.0 * 1024.0,
        "g" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return Err(invalid()),
    };
    let bytes = value * factor;
    // 2^53 bytes is far beyond any machine and still exact as a float.
    if !bytes.is_finite() || bytes < 1.0 || bytes > 9_007_199_254_740_992.0 {
        return Err(format!("`memory` `{}` is out of range", inline(text, 32)));
    }
    Ok(bytes.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sizes_parse_with_binary_and_decimal_units() {
        assert_eq!(memory_bytes("8GiB"), Ok(8 << 30));
        assert_eq!(memory_bytes(" 512 MiB "), Ok(512 << 20));
        assert_eq!(memory_bytes("1.5g"), Ok(3 << 29));
        assert_eq!(memory_bytes("2GB"), Ok(2_000_000_000));
        assert_eq!(memory_bytes("4096"), Ok(4096));
        for bad in [
            "",
            "GiB",
            "8 gigs",
            "-1GiB",
            "0",
            "1e3",
            "99999999TiB",
            "1.2.3G",
        ] {
            assert!(memory_bytes(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn platforms_parse_with_and_without_architecture() {
        let linux = platform_selector("linux").unwrap();
        assert_eq!((linux.os, linux.arch), (Some(Os::Linux), None));
        let mac = platform_selector("macOS-arm64").unwrap();
        assert_eq!((mac.os, mac.arch), (Some(Os::Macos), Some(Arch::Aarch64)));
        let windows = platform_selector("windows-x86_64").unwrap();
        assert_eq!(windows.arch, Some(Arch::X86_64));
        assert!(platform_selector("plan9").is_err());
        assert!(platform_selector("linux-sparc").is_err());
        assert!(platform_selector("aarch64").is_err());
    }
}

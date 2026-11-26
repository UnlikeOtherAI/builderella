//! `get_artifacts`: list a job's outputs and optionally download them.

use super::render::{bytes, tag};
use super::{Call, Failure, Output, ToolResult, flag_arg, job_id_arg, job_id_schema, text_arg};
use crate::paths::{join_relative, obstruction};
use crate::sanitize::inline;
use bella_core::api::ArtifactView;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;

/// Most artifacts listed in one result.
const MAX_LISTED: usize = 100;
/// Most files fetched by one call.
const MAX_DOWNLOADS: usize = 200;
const MAX_PATH_CHARS: usize = 300;
const MAX_NAMED: usize = 8;

pub(super) fn input() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["job_id"],
        "properties": {
            "job_id": job_id_schema(),
            "download": {
                "type": "boolean",
                "description": "true also fetches the artifacts into `dest`. Default false: list only."
            },
            "dest": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4096,
                "description": "With `download`: absolute path of the directory to write into, inside a directory this server was started for. Created when missing. There is no default. Existing files are never overwritten."
            },
            "paths": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_DOWNLOADS,
                "items": {"type": "string", "minLength": 1, "maxLength": 1024},
                "description": "With `download`: fetch only these artifact paths, exactly as listed. All artifacts when omitted."
            }
        }
    })
}

pub(super) fn output() -> Value {
    json!({
        "type": "object",
        "required": ["job_id", "total", "artifacts", "omitted"],
        "properties": {
            "job_id": {"type": "string"},
            "total": {"type": "integer", "description": "Number of artifacts the job produced."},
            "artifacts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["path", "kind", "size", "digest", "attempt_id", "snapshot_id"],
                    "properties": {
                        "path": {"type": "string"},
                        "kind": {"type": "string", "description": "artifact or test_report."},
                        "size": {"type": "integer", "description": "Bytes."},
                        "digest": {"type": "string", "description": "SHA-256 of the content, lowercase hex."},
                        "attempt_id": {"type": "string", "description": "The attempt that produced it."},
                        "snapshot_id": {"type": "string", "description": "The source snapshot it was built from."}
                    }
                }
            },
            "omitted": {"type": "integer", "description": "Artifacts not listed because the list is bounded."},
            "dest": {"type": "string", "description": "With `download`: the directory written into."},
            "downloaded": {
                "type": "array",
                "description": "With `download`: the files that were written.",
                "items": {
                    "type": "object",
                    "required": ["path", "file", "size", "digest"],
                    "properties": {
                        "path": {"type": "string"},
                        "file": {"type": "string"},
                        "size": {"type": "integer"},
                        "digest": {"type": "string"}
                    }
                }
            },
            "downloaded_count": {"type": "integer"}
        }
    })
}

fn shown(path: &str) -> String {
    inline(path, MAX_PATH_CHARS)
}

fn listing(artifact: &ArtifactView) -> Value {
    json!({
        "path": shown(&artifact.path),
        "kind": tag(&artifact.kind),
        "size": artifact.size,
        "digest": artifact.digest.as_str(),
        "attempt_id": artifact.attempt_id.as_str(),
        "snapshot_id": artifact.snapshot_id.to_string(),
    })
}

pub(super) fn get(call: &Call<'_>, arguments: &Map<String, Value>) -> ToolResult {
    let id = job_id_arg(arguments)?;
    let download = flag_arg(arguments, "download") == Some(true);
    let dest = text_arg(arguments, "dest");
    let wanted: Option<Vec<&str>> = arguments
        .get("paths")
        .and_then(Value::as_array)
        .map(|paths| paths.iter().filter_map(Value::as_str).collect());
    if !download && (dest.is_some() || wanted.is_some()) {
        return Err(Failure::Invalid(
            "`dest` and `paths` apply only with `download: true`".into(),
        ));
    }
    if download && dest.is_none() {
        return Err(Failure::Invalid(
            "`download: true` needs `dest`, the directory to write into; there is no default".into(),
        ));
    }

    let artifacts = call.fleet.artifacts(&id)?;
    let total = artifacts.len();
    let mut fields = Map::new();
    let mut lines = vec![format!("job {id}: {total} artifact(s)")];
    for artifact in artifacts.iter().take(MAX_LISTED) {
        lines.push(format!(
            "- {} ({}, {}, sha256 {}, attempt {}, snapshot {})",
            shown(&artifact.path),
            tag(&artifact.kind),
            bytes(artifact.size),
            artifact.digest.short(),
            artifact.attempt_id,
            artifact.snapshot_id.digest().short()
        ));
    }
    let omitted = total.saturating_sub(MAX_LISTED);
    if omitted > 0 {
        lines.push(format!("({omitted} more not listed)"));
    }
    fields.insert("job_id".into(), json!(id.as_str()));
    fields.insert("total".into(), json!(total));
    fields.insert(
        "artifacts".into(),
        Value::Array(artifacts.iter().take(MAX_LISTED).map(listing).collect()),
    );
    fields.insert("omitted".into(), json!(omitted));

    if let Some(dest) = dest {
        let selected = select(&artifacts, wanted.as_deref())?;
        let dest = call
            .config
            .roots
            .writable_dir("dest", dest)
            .map_err(Failure::Invalid)?;
        let written = fetch(call, &id, &selected, &dest)?;
        lines.push(format!(
            "downloaded {} file(s) into {}",
            written.len(),
            dest.display()
        ));
        fields.insert("dest".into(), json!(dest.display().to_string()));
        fields.insert("downloaded_count".into(), json!(written.len()));
        fields.insert(
            "downloaded".into(),
            Value::Array(written.into_iter().take(MAX_LISTED).collect()),
        );
    }
    Ok(Output::new(lines, fields))
}

/// The artifacts to fetch, one per path. When several attempts produced the
/// same path, the newest attempt's artifact stands for it, matching what
/// `FleetOps::fetch_artifact` fetches.
fn select<'a>(
    artifacts: &'a [ArtifactView],
    wanted: Option<&[&str]>,
) -> Result<Vec<&'a ArtifactView>, Failure> {
    let mut newest: BTreeMap<&str, &ArtifactView> = BTreeMap::new();
    for artifact in artifacts {
        let slot = newest.entry(artifact.path.as_str()).or_insert(artifact);
        if artifact.attempt_id > slot.attempt_id {
            *slot = artifact;
        }
    }
    let selected: Vec<&ArtifactView> = match wanted {
        None => newest.into_values().collect(),
        Some(paths) => {
            let unknown: Vec<String> = paths
                .iter()
                .filter(|path| !newest.contains_key(**path))
                .take(MAX_NAMED)
                .map(|path| format!("`{}`", shown(path)))
                .collect();
            if !unknown.is_empty() {
                return Err(Failure::Invalid(format!(
                    "`paths` names artifacts this job does not have: {}. Use the paths exactly as get_artifacts lists them",
                    unknown.join(", ")
                )));
            }
            let mut chosen: BTreeMap<&str, &ArtifactView> = BTreeMap::new();
            for path in paths {
                if let Some(artifact) = newest.get(*path) {
                    chosen.insert(*path, *artifact);
                }
            }
            chosen.into_values().collect()
        }
    };
    if selected.is_empty() {
        return Err(Failure::Invalid(
            "this job has no artifacts to download".into(),
        ));
    }
    if selected.len() > MAX_DOWNLOADS {
        return Err(Failure::Invalid(format!(
            "this would download {} files; one call fetches at most {MAX_DOWNLOADS}, so name the ones you need in `paths`",
            selected.len()
        )));
    }
    Ok(selected)
}

/// Check every target first, then fetch. Nothing is fetched when any target
/// already exists, so a clash never leaves a partial download behind.
fn fetch(
    call: &Call<'_>,
    id: &bella_core::JobId,
    selected: &[&ArtifactView],
    dest: &Path,
) -> Result<Vec<Value>, Failure> {
    let mut conflicts: Vec<String> = Vec::new();
    for artifact in selected {
        let target = join_relative(dest, &artifact.path).map_err(|reason| {
            Failure::Other(format!(
                "artifact path `{}` cannot be written safely ({reason}); nothing was downloaded",
                shown(&artifact.path)
            ))
        })?;
        if let Some(blocking) = obstruction(dest, &target) {
            conflicts.push(blocking.display().to_string());
        }
    }
    if !conflicts.is_empty() {
        conflicts.sort();
        conflicts.dedup();
        let count = conflicts.len();
        let named: Vec<String> = conflicts
            .into_iter()
            .take(MAX_NAMED)
            .map(|path| format!("`{}`", shown(&path)))
            .collect();
        return Err(Failure::Other(format!(
            "nothing was downloaded, because {count} path(s) already exist in `dest` and existing files are never overwritten: {}{}. Choose an empty `dest` or name other `paths`.",
            named.join(", "),
            if count > MAX_NAMED { ", ..." } else { "" }
        )));
    }

    let canonical_dest = fs::canonicalize(dest).ok();
    let mut written: Vec<Value> = Vec::new();
    let already = |written: &[Value]| {
        if written.is_empty() {
            "No file was written.".to_string()
        } else {
            format!(
                "{} file(s) were written to `{}` before this and were left in place.",
                written.len(),
                dest.display()
            )
        }
    };
    for artifact in selected {
        if call.shutdown.load(Ordering::SeqCst) {
            return Err(Failure::Abandoned);
        }
        let file = match call.fleet.fetch_artifact(id, &artifact.path, dest) {
            Ok(file) => file,
            Err(error) => {
                return Err(Failure::Api {
                    error,
                    note: Some(format!(
                        "The download stopped at `{}`. {}",
                        shown(&artifact.path),
                        already(&written)
                    )),
                });
            }
        };
        // Judge the reported file by where it really is.
        let inside = match (&canonical_dest, fs::canonicalize(&file)) {
            (Some(dest), Ok(real)) => real.starts_with(dest),
            _ => false,
        };
        if !inside {
            return Err(Failure::Other(format!(
                "the download of `{}` reported a file outside `dest`; stopping. {}",
                shown(&artifact.path),
                already(&written)
            )));
        }
        written.push(json!({
            "path": shown(&artifact.path),
            "file": file.display().to_string(),
            "size": artifact.size,
            "digest": artifact.digest.as_str(),
        }));
    }
    Ok(written)
}

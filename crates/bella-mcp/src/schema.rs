//! Validation of tool arguments against the tool's own published schema.
//!
//! The schemas this crate publishes use a small, fixed part of JSON Schema
//! 2020-12. Validating with the same document that `tools/list` returns
//! means the advertised contract and the enforced one cannot drift apart.

use serde_json::Value;

/// Assertion keywords [`validate`] enforces.
#[cfg(test)]
pub(crate) const ASSERTIONS: &[&str] = &[
    "type",
    "enum",
    "properties",
    "required",
    "additionalProperties",
    "maxProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
];

/// Keywords that describe a value without constraining it.
#[cfg(test)]
pub(crate) const ANNOTATIONS: &[&str] = &["description", "title", "default"];

/// Check `instance` against `schema`. The error names the offending
/// argument in words an agent can act on.
pub(crate) fn validate(schema: &Value, instance: &Value) -> Result<(), String> {
    check(schema, instance, "")
}

fn check(schema: &Value, instance: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        // `true` accepts everything, `false` nothing.
        return match schema {
            Value::Bool(false) => Err(format!("{} is not allowed", subject(path))),
            _ => Ok(()),
        };
    };

    if let Some(expected) = schema.get("type") {
        let names: Vec<&str> = match expected {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.iter().any(|name| has_type(instance, name)) {
            return Err(format!(
                "{} must be {}, got {}",
                subject(path),
                describe_types(&names),
                type_name(instance)
            ));
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(instance)
    {
        let choices: Vec<String> = allowed.iter().map(Value::to_string).collect();
        return Err(format!(
            "{} must be one of {}",
            subject(path),
            choices.join(", ")
        ));
    }

    match instance {
        Value::Object(map) => {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !map.contains_key(name) {
                        return Err(format!(
                            "missing required argument {}",
                            subject(&join(path, name))
                        ));
                    }
                }
            }
            if let Some(max) = schema.get("maxProperties").and_then(Value::as_u64)
                && map.len() as u64 > max
            {
                return Err(format!(
                    "{} has more than {max} entries",
                    subject(path)
                ));
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            for (name, value) in map {
                let child = join(path, name);
                match properties.and_then(|p| p.get(name)) {
                    Some(sub) => check(sub, value, &child)?,
                    None => match additional {
                        Some(Value::Bool(false)) => {
                            return Err(format!("unknown argument {}", subject(&child)));
                        }
                        Some(sub) => check(sub, value, &child)?,
                        None => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < min
            {
                return Err(format!(
                    "{} needs at least {min} item(s)",
                    subject(path)
                ));
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
                && items.len() as u64 > max
            {
                return Err(format!(
                    "{} has more than {max} items",
                    subject(path)
                ));
            }
            if let Some(sub) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check(sub, item, &format!("{path}[{index}]"))?;
                }
            }
        }
        Value::String(text) => {
            let length = text.chars().count() as u64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
                && length < min
            {
                return Err(if min == 1 {
                    format!("{} must not be empty", subject(path))
                } else {
                    format!("{} must be at least {min} characters", subject(path))
                });
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
                && length > max
            {
                return Err(format!(
                    "{} must be at most {max} characters",
                    subject(path)
                ));
            }
        }
        Value::Number(number) => {
            let value = number.as_f64().unwrap_or(f64::NAN);
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
                && (value.is_nan() || value < min)
            {
                return Err(format!("{} must be at least {min}", subject(path)));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
                && (value.is_nan() || value > max)
            {
                return Err(format!("{} must be at most {max}", subject(path)));
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
    Ok(())
}

fn has_type(instance: &Value, name: &str) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "number" => instance.is_number(),
        "integer" => match instance {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        _ => false,
    }
}

fn type_name(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn describe_types(names: &[&str]) -> String {
    let words: Vec<&str> = names
        .iter()
        .map(|name| match *name {
            "object" => "an object",
            "array" => "an array",
            "string" => "a string",
            "boolean" => "a boolean",
            "null" => "null",
            "number" => "a number",
            "integer" => "an integer",
            other => other,
        })
        .collect();
    words.join(" or ")
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

fn subject(path: &str) -> String {
    if path.is_empty() {
        "the arguments".to_string()
    } else {
        format!("`{path}`")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "minLength": 1, "maxLength": 8},
                "count": {"type": "integer", "minimum": 0, "maximum": 10},
                "mode": {"type": "string", "enum": ["a", "b"]},
                "cmd": {"type": ["string", "array"], "items": {"type": "string"}, "minItems": 1, "maxItems": 2},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "maxProperties": 1},
                "ratio": {"type": "number", "minimum": 0.5}
            }
        })
    }

    #[test]
    fn accepts_conforming_values() {
        let ok = json!({"id": "x", "count": 3, "mode": "b", "cmd": ["a", "b"], "env": {"K": "v"}, "ratio": 0.5});
        assert_eq!(validate(&schema(), &ok), Ok(()));
        assert_eq!(validate(&schema(), &json!({"id": "x", "cmd": "sh"})), Ok(()));
        assert_eq!(validate(&schema(), &json!({"id": "x", "count": 2.0})), Ok(()));
    }

    #[test]
    fn rejects_each_kind_of_violation() {
        let cases = [
            (json!({}), "missing required argument `id`"),
            (json!({"id": "x", "extra": 1}), "unknown argument `extra`"),
            (json!({"id": 1}), "`id` must be a string, got a number"),
            (json!({"id": ""}), "`id` must not be empty"),
            (json!({"id": "123456789"}), "`id` must be at most 8 characters"),
            (json!({"id": "x", "count": 1.5}), "`count` must be an integer, got a number"),
            (json!({"id": "x", "count": -1}), "`count` must be at least 0"),
            (json!({"id": "x", "count": 11}), "`count` must be at most 10"),
            (json!({"id": "x", "mode": "c"}), "`mode` must be one of \"a\", \"b\""),
            (json!({"id": "x", "cmd": 5}), "`cmd` must be a string or an array, got a number"),
            (json!({"id": "x", "cmd": []}), "`cmd` needs at least 1 item(s)"),
            (json!({"id": "x", "cmd": ["a", "b", "c"]}), "`cmd` has more than 2 items"),
            (json!({"id": "x", "cmd": ["a", 2]}), "`cmd[1]` must be a string, got a number"),
            (json!({"id": "x", "env": {"K": 1}}), "`env.K` must be a string, got a number"),
            (json!({"id": "x", "env": {"A": "1", "B": "2"}}), "`env` has more than 1 entries"),
            (json!({"id": "x", "ratio": 0.25}), "`ratio` must be at least 0.5"),
            (json!([]), "the arguments must be an object, got an array"),
        ];
        for (instance, expected) in cases {
            assert_eq!(validate(&schema(), &instance), Err(expected.to_string()));
        }
    }
}

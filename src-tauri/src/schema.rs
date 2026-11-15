use crate::model::*;
use serde_json::Value;
// Only the JSON Schema vocabulary used by the admitted MCP catalogs. Unknown refs fail closed.
pub fn validate(schema: &Value, value: &Value) -> Result<()> {
    if schema.get("$ref").is_some() {
        return Err("unsupported schema reference; refresh/upgrade the admitted catalog".into());
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        if !options.iter().any(|s| validate(s, value).is_ok()) {
            return Err("arguments do not match anyOf".into());
        }
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
        if options
            .iter()
            .filter(|s| validate(s, value).is_ok())
            .count()
            != 1
        {
            return Err("arguments do not match oneOf".into());
        }
    }
    if let Some(options) = schema.get("allOf").and_then(Value::as_array) {
        for s in options {
            validate(s, value)?
        }
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array) {
        if !options.contains(value) {
            return Err("invalid enum value".into());
        }
    }
    if let Some(c) = schema.get("const") {
        if c != value {
            return Err("invalid constant".into());
        }
    }
    let matches = |t: &str| match t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_f64().map(|v| v.fract() == 0.0).unwrap_or(false),
        _ => false,
    };
    if let Some(t) = schema.get("type") {
        let valid = if let Some(t) = t.as_str() {
            matches(t)
        } else {
            t.as_array()
                .map(|ts| ts.iter().any(|t| t.as_str().map(&matches).unwrap_or(false)))
                .unwrap_or(false)
        };
        if !valid {
            return Err("argument type mismatch".into());
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required {
                if let Some(key) = key.as_str() {
                    if !object.contains_key(key) {
                        return Err(format!("missing argument {key}"));
                    }
                }
            }
        }
        for (key, v) in object {
            if let Some(s) = schema.get("properties").and_then(|p| p.get(key)) {
                validate(s, v).map_err(|e| format!("{key}: {e}"))?
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("unknown argument {key}"));
            }
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(s) = schema.get("items") {
            for item in items {
                validate(s, item)?
            }
        }
        for (name, lower) in [("minItems", true), ("maxItems", false)] {
            if let Some(n) = schema.get(name).and_then(Value::as_u64) {
                if lower && items.len() < (n as usize) || !lower && items.len() > (n as usize) {
                    return Err(name.into());
                }
            }
        }
    }
    if let Some(n) = value.as_f64() {
        for (name, lower, exclusive) in [
            ("minimum", true, false),
            ("maximum", false, false),
            ("exclusiveMinimum", true, true),
            ("exclusiveMaximum", false, true),
        ] {
            if let Some(bound) = schema.get(name).and_then(Value::as_f64) {
                if lower && (n < bound || exclusive && n == bound)
                    || !lower && (n > bound || exclusive && n == bound)
                {
                    return Err(name.into());
                }
            }
        }
    }
    if let Some(s) = value.as_str() {
        for (name, lower) in [("minLength", true), ("maxLength", false)] {
            if let Some(n) = schema.get(name).and_then(Value::as_u64) {
                let count = s.chars().count();
                if lower && count < (n as usize) || !lower && count > (n as usize) {
                    return Err(name.into());
                }
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rejects_required_and_extras() {
        let s = json!({"type":"object","required":["device"],"properties":{"device":{"type":"string"}},"additionalProperties":false});
        assert!(validate(&s, &json!({"device":"a"})).is_ok());
        assert!(validate(&s, &json!({})).is_err());
        assert!(validate(&s, &json!({"device":"a","target":"x"})).is_err());
    }
}

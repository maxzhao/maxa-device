use crate::model::*;
use serde_json::Value;
// Only the JSON Schema vocabulary used by the admitted MCP catalogs. Fail closed
// before alternatives so an unsupported branch cannot silently pass via another.
fn check_schema(schema: &Value) -> Result<()> {
    if schema.is_boolean() {
        return Ok(());
    }
    let object = schema
        .as_object()
        .ok_or("schema must be an object or boolean")?;
    for (keyword, child) in object {
        match keyword.as_str() {
            "description" | "title" | "default" | "examples" | "deprecated" | "readOnly"
            | "writeOnly" | "$schema" | "$id" | "$comment" | "type" | "required" | "enum"
            | "const" | "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum"
            | "minItems" | "maxItems" | "minLength" | "maxLength" | "minProperties"
            | "maxProperties" => (),
            "properties" => {
                for s in child
                    .as_object()
                    .ok_or("properties must be an object")?
                    .values()
                {
                    check_schema(s)?;
                }
            }
            "additionalProperties" | "propertyNames" | "items" => check_schema(child)?,
            "anyOf" | "oneOf" | "allOf" => {
                let branches = child
                    .as_array()
                    .filter(|a| !a.is_empty())
                    .ok_or("alternatives must be a nonempty array")?;
                for s in branches {
                    check_schema(s)?;
                }
            }
            "pattern" => {
                regex::Regex::new(child.as_str().ok_or("pattern must be a string")?)
                    .map_err(|e| format!("unsupported/invalid pattern: {e}"))?;
            }
            _ => {
                return Err(format!(
                    "unsupported schema keyword {keyword}; refresh/upgrade the admitted catalog"
                ))
            }
        }
    }
    Ok(())
}
pub fn validate(schema: &Value, value: &Value) -> Result<()> {
    check_schema(schema)?;
    if schema == &Value::Bool(true) {
        return Ok(());
    }
    if schema == &Value::Bool(false) {
        return Err("value is not permitted".into());
    }
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
            if let Some(names) = schema.get("propertyNames") {
                validate(names, &Value::String(key.clone()))
                    .map_err(|e| format!("property name {key}: {e}"))?;
            }
            if let Some(s) = schema.get("properties").and_then(|p| p.get(key)) {
                validate(s, v).map_err(|e| format!("{key}: {e}"))?
            } else if let Some(s) = schema.get("additionalProperties") {
                validate(s, v).map_err(|e| format!("{key}: {e}"))?;
            }
        }
        for (name, lower) in [("minProperties", true), ("maxProperties", false)] {
            if let Some(n) = schema.get(name).and_then(Value::as_u64) {
                if lower && object.len() < n as usize || !lower && object.len() > n as usize {
                    return Err(name.into());
                }
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
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str) {
            if !regex::Regex::new(pattern)
                .map_err(|e| e.to_string())?
                .is_match(s)
            {
                return Err("pattern".into());
            }
        }
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
    fn property_names_compose_with_value_and_other_assertions() {
        let s = json!({"type":"object","propertyNames":{"type":"string","pattern":"^[a-z]+$"},"additionalProperties":{"type":"integer"}});
        assert!(validate(&s, &json!({"key":1})).is_ok());
        assert!(validate(&s, &json!({"UPPER":1})).is_err());
        assert!(validate(&s, &json!({"key":"wrong"})).is_err());
        assert!(validate(&json!({"propertyNames":false}), &json!({})).is_ok());
        assert!(validate(&json!({"propertyNames":false}), &json!({"key":1})).is_err());
        assert!(validate(
            &json!({"propertyNames":{"type":"string"},"additionalProperties":false}),
            &json!({"key":1})
        )
        .is_err());
        assert!(validate(&json!({"anyOf":[{}, {"unknownKeyword":true}]}), &json!({})).is_err());
        let tools: Value =
            serde_json::from_str(include_str!("catalogs/mobile-tools.json")).unwrap();
        let batch = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "mobile_batch_commands")
            .unwrap();
        assert!(validate(
            &batch["inputSchema"],
            &json!({"device":"probe","steps":[{"name":"mobile_get_orientation","arguments":{}}]})
        )
        .is_ok());
    }
    #[test]
    fn rejects_required_and_extras() {
        let s = json!({"type":"object","required":["device"],"properties":{"device":{"type":"string"}},"additionalProperties":false});
        assert!(validate(&s, &json!({"device":"a"})).is_ok());
        assert!(validate(&s, &json!({})).is_err());
        assert!(validate(&s, &json!({"device":"a","target":"x"})).is_err());
    }
}

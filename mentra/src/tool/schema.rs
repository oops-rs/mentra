//! Checks a tool call's input against the schema the tool declared.
//!
//! A tool publishes an `input_schema` to the model and, until this ran, nothing
//! ever compared a call against it. A model that omitted a required field or
//! sent a string where a number belonged reached the tool's own code, which
//! either produced a confusing deserialization error or — for a tool reading
//! fields loosely — did the wrong thing quietly.
//!
//! This is deliberately not a complete JSON Schema implementation. It covers
//! what models actually get wrong — a missing required field, a wrong scalar
//! type, a value outside an enum, a misspelled property — and ignores the rest
//! rather than rejecting a call over a keyword it does not understand. A schema
//! feature not implemented here must never turn a valid call into a failure.

use serde_json::Value;

/// Validates `input` against `schema`, returning what a model should be told.
pub(crate) fn validate_tool_input(schema: &Value, input: &Value) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Some(error) = root_shape_error(schema, input) {
        // Nothing below the root can be checked against a value that is not
        // the shape the root describes.
        return Err(error);
    }
    check(schema, input, "", &mut errors);
    if errors.is_empty() {
        return Ok(());
    }
    Err(errors.join("; "))
}

/// Rejects a call whose arguments are not an object, for a root schema that
/// describes one without saying `type`.
///
/// A schema that declares `properties` or `required` is describing an object
/// whether or not it spells that out, and `required` is otherwise skipped for a
/// non-object -- which is correct JSON Schema and wrong for a tool call, whose
/// arguments are an object by every provider's contract. Without this, a schema
/// that omitted `type` let a bare string or array through untouched, and a host
/// binding a schema that arrives as *data* rather than code had to re-check the
/// shape itself.
///
/// Deliberately narrow. It applies at the root only, and only when the schema
/// shows object intent: an empty schema, or `true`, still accepts anything,
/// because a schema that describes nothing is not describing an object either.
fn root_shape_error(schema: &Value, input: &Value) -> Option<String> {
    let schema = schema.as_object()?;
    if schema.contains_key("type") || input.is_object() {
        return None;
    }
    if !schema.contains_key("properties") && !schema.contains_key("required") {
        return None;
    }
    Some(format!(
        "input should be an object, got {}",
        json_type_name(input)
    ))
}

fn check(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else {
        // `true`, or something that is not a schema at all: nothing to check.
        return;
    };

    if let Some(declared) = schema.get("type")
        && !type_matches(declared, value)
    {
        errors.push(format!(
            "{} should be {}, got {}",
            describe(path),
            describe_type(declared),
            json_type_name(value)
        ));
        // A value of the wrong type cannot usefully be checked further.
        return;
    }

    if let Some(Value::Array(allowed)) = schema.get("enum")
        && !allowed.contains(value)
    {
        errors.push(format!(
            "{} should be one of {}",
            describe(path),
            allowed
                .iter()
                .map(|option| option.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let properties = schema.get("properties").and_then(Value::as_object);

    if let Some(Value::Array(required)) = schema.get("required")
        && let Some(object) = value.as_object()
    {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                errors.push(format!("{} is required", describe(&join(path, name))));
            }
        }
    }

    let Some(object) = value.as_object() else {
        return;
    };

    if schema.get("additionalProperties") == Some(&Value::Bool(false))
        && let Some(properties) = properties
    {
        for name in object.keys() {
            if !properties.contains_key(name) {
                errors.push(format!(
                    "{} is not a property of this tool",
                    describe(&join(path, name))
                ));
            }
        }
    }

    let Some(properties) = properties else {
        return;
    };
    for (name, property_schema) in properties {
        if let Some(present) = object.get(name) {
            check(property_schema, present, &join(path, name), errors);
        }
    }
}

/// Whether `value` satisfies a `type` keyword, which may name one type or
/// several.
fn type_matches(declared: &Value, value: &Value) -> bool {
    match declared {
        Value::String(name) => matches_type_name(name, value),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| matches_type_name(name, value)),
        // An unrecognized `type` is not something to fail a call over.
        _ => true,
    }
}

fn matches_type_name(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        // A whole number arriving as a JSON float is the same number, and
        // refusing it would fail calls that are correct.
        "integer" => value.as_i64().is_some() || value.as_f64().is_some_and(|n| n.fract() == 0.0),
        "number" => value.is_number(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn describe_type(declared: &Value) -> String {
    match declared {
        Value::String(name) => name.clone(),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        other => other.to_string(),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

fn describe(path: &str) -> String {
    if path.is_empty() {
        "the input".to_string()
    } else {
        format!("'{path}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "mode": {"enum": ["read", "write"]},
                "options": {
                    "type": "object",
                    "properties": {"recursive": {"type": "boolean"}}
                }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn a_valid_call_passes() {
        let input = json!({"path": "a.rs", "limit": 10, "mode": "read"});
        assert_eq!(validate_tool_input(&schema(), &input), Ok(()));
    }

    #[test]
    fn a_missing_required_field_is_named() {
        let error =
            validate_tool_input(&schema(), &json!({"limit": 1})).expect_err("path is required");
        assert!(error.contains("'path' is required"), "{error}");
    }

    #[test]
    fn a_wrong_scalar_type_says_what_was_expected_and_what_arrived() {
        let error =
            validate_tool_input(&schema(), &json!({"path": 7})).expect_err("path must be a string");
        assert!(
            error.contains("'path' should be string, got number"),
            "{error}"
        );
    }

    #[test]
    fn a_whole_number_satisfies_an_integer_however_it_arrived() {
        // Providers routinely serialize 10 as 10.0, and failing that call would
        // be failing a correct one.
        let input = json!({"path": "a.rs", "limit": 10.0});
        assert_eq!(validate_tool_input(&schema(), &input), Ok(()));

        let error = validate_tool_input(&schema(), &json!({"path": "a.rs", "limit": 1.5}))
            .expect_err("1.5 is not an integer");
        assert!(error.contains("'limit'"), "{error}");
    }

    #[test]
    fn a_value_outside_an_enum_lists_the_options() {
        let error = validate_tool_input(&schema(), &json!({"path": "a.rs", "mode": "delete"}))
            .expect_err("delete is not a mode");
        assert!(error.contains("'mode' should be one of"), "{error}");
    }

    #[test]
    fn a_nested_property_is_checked_too() {
        let error = validate_tool_input(
            &schema(),
            &json!({"path": "a.rs", "options": {"recursive": "yes"}}),
        )
        .expect_err("recursive must be a boolean");
        assert!(error.contains("'options.recursive'"), "{error}");
    }

    #[test]
    fn an_unknown_property_is_only_refused_when_the_schema_says_so() {
        let permissive = json!({"path": "a.rs", "typo": 1});
        assert_eq!(validate_tool_input(&schema(), &permissive), Ok(()));

        let mut strict = schema();
        strict["additionalProperties"] = json!(false);
        let error = validate_tool_input(&strict, &permissive).expect_err("typo is not a property");
        assert!(error.contains("'typo' is not a property"), "{error}");
    }

    #[test]
    fn a_schema_keyword_this_does_not_implement_never_fails_a_call() {
        // The rule that keeps this safe to run on every call: an unimplemented
        // feature must never turn a valid call into a failure.
        let exotic = json!({
            "type": "object",
            "properties": {"path": {"type": "string", "pattern": "^/", "minLength": 40}},
            "allOf": [{"required": ["nothing"]}]
        });
        assert_eq!(
            validate_tool_input(&exotic, &json!({"path": "a.rs"})),
            Ok(())
        );
    }

    #[test]
    fn several_problems_are_reported_together() {
        let error = validate_tool_input(&schema(), &json!({"limit": "ten", "mode": "delete"}))
            .expect_err("three things are wrong");
        assert!(error.contains("'path' is required"), "{error}");
        assert!(error.contains("'limit'"), "{error}");
        assert!(error.contains("'mode'"), "{error}");
    }
}

#[cfg(test)]
mod root_shape_tests {
    use super::*;
    use serde_json::json;

    /// A tool binding whose schema arrives as data rather than code — a
    /// workspace manifest — may omit `type` at the root. `required` is skipped
    /// for a non-object, which is correct JSON Schema and useless here: the
    /// program behind the tool receives arguments it never agreed to accept.
    #[test]
    fn a_schema_without_a_type_still_requires_an_object() {
        let schema = json!({
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        });

        let error = validate_tool_input(&schema, &json!("just a string"))
            .expect_err("a string cannot satisfy a schema describing properties");
        assert!(
            error.contains("should be an object"),
            "the message should say what was wrong: {error}"
        );

        assert_eq!(
            validate_tool_input(&schema, &json!({"path": "a.rs"})),
            Ok(()),
            "an object satisfying the schema still passes"
        );
        assert!(
            validate_tool_input(&schema, &json!({})).is_err(),
            "and `required` is now actually enforced for such a schema"
        );
    }

    /// The narrowness is the point: a schema that describes nothing is not
    /// describing an object, so it keeps accepting anything. Rejecting here
    /// would turn a valid call into a failure over a keyword that was never
    /// written.
    #[test]
    fn a_schema_that_describes_nothing_still_accepts_anything() {
        for schema in [json!({}), json!(true), json!({"description": "free-form"})] {
            for input in [json!("text"), json!(7), json!([1, 2]), json!(null)] {
                assert_eq!(
                    validate_tool_input(&schema, &input),
                    Ok(()),
                    "schema {schema} must not reject {input}"
                );
            }
        }
    }

    /// A root that does say `type` keeps reporting through the existing path,
    /// which already names the declared type.
    #[test]
    fn a_declared_root_type_is_reported_by_the_type_check() {
        let schema = json!({"type": "object", "properties": {}});
        let error =
            validate_tool_input(&schema, &json!([])).expect_err("an array is not an object");
        assert!(error.contains("should be object"), "got {error}");
    }
}

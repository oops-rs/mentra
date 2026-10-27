//! Host-supplied top-level fields added to every request body an
//! OpenAI-compatible provider sends.
//!
//! A gateway or a vendor often takes a parameter mentra does not model:
//! LiteLLM forwards an `extra_body` object to the upstream model, DeepSeek
//! turns thinking off with `thinking`, and so on. These fields let a host say
//! that once per provider instead of mentra learning each one.
//!
//! They only ever add. A field mentra already set on a request is never
//! replaced: the request fails instead, naming the field, because a silent
//! override would change what mentra believes it sent (the model, the tools,
//! the tool choice), and a silently dropped field would hide the setting the
//! host asked for.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::ProviderError;

/// Top-level JSON fields merged into each request body.
pub type RequestFields = Map<String, Value>;

/// Serializes `body` and adds `fields` to it.
///
/// Fails when `body` already has one of the fields, or is not a JSON object.
pub(crate) fn body_with_request_fields<T: Serialize>(
    body: &T,
    fields: &RequestFields,
) -> Result<Value, ProviderError> {
    let body = serde_json::to_value(body).map_err(ProviderError::Serialize)?;
    merge_request_fields(body, fields)
}

/// Adds `fields` to an already serialized request body.
pub(crate) fn merge_request_fields(
    mut body: Value,
    fields: &RequestFields,
) -> Result<Value, ProviderError> {
    if fields.is_empty() {
        return Ok(body);
    }
    let Some(object) = body.as_object_mut() else {
        return Err(ProviderError::InvalidRequest(
            "request fields need a JSON object request body".to_string(),
        ));
    };
    let mut collisions = fields
        .keys()
        .filter(|name| object.contains_key(name.as_str()))
        .map(|name| format!("`{name}`"))
        .peekable();
    if collisions.peek().is_some() {
        let names = collisions.collect::<Vec<_>>().join(", ");
        return Err(ProviderError::InvalidRequest(format!(
            "extra request field(s) {names} would replace what mentra set on this request; \
             request fields may only add fields"
        )));
    }
    object.extend(
        fields
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    Ok(body)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fields(value: Value) -> RequestFields {
        value.as_object().cloned().expect("an object")
    }

    #[test]
    fn fields_are_added_at_the_top_level() {
        let body = json!({"model": "m", "stream": true});
        let merged = merge_request_fields(
            body,
            &fields(json!({"extra_body": {"thinking": {"type": "disabled"}}})),
        )
        .expect("nothing collides");
        assert_eq!(
            merged,
            json!({
                "model": "m",
                "stream": true,
                "extra_body": {"thinking": {"type": "disabled"}}
            })
        );
    }

    #[test]
    fn a_field_mentra_set_is_refused_rather_than_replaced() {
        let body = json!({"model": "m", "tool_choice": "auto"});
        let error = merge_request_fields(
            body,
            &fields(json!({"tool_choice": "none", "model": "other", "seed": 1})),
        )
        .expect_err("two fields collide");
        let message = error.to_string();
        assert!(message.contains("`model`"), "{message}");
        assert!(message.contains("`tool_choice`"), "{message}");
        assert!(!message.contains("`seed`"), "{message}");
    }

    #[test]
    fn no_fields_leave_the_body_untouched() {
        let body = json!("not even an object");
        assert_eq!(
            merge_request_fields(body.clone(), &RequestFields::new()).unwrap(),
            body
        );
    }
}

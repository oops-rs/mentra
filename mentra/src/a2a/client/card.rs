//! Reading an Agent Card, as written by agents that are not this crate.
//!
//! A v1.0 card's security requirement is the proto's
//! `SecurityRequirement { map<string, StringList> schemes }`, so its JSON is
//! `{"schemes": {"bearer": {"list": ["scope"]}}}`. proto3 JSON leaves an
//! empty repeated field out, which makes a requirement with no scopes —
//! the common bearer case — `{"schemes": {"bearer": {}}}`. `a2a-lf` 0.4.1
//! refuses that shape, so a card from the official Python SDK that asks for
//! a bearer token was not a card at all. The missing list is put back before
//! the card is decoded; nothing else is touched.

use a2a_lf::AgentCard;
use serde_json::Value;

use super::A2aError;

/// Decodes a card, accepting a security requirement whose scope list a
/// proto3 encoder left out.
pub(crate) fn decode(mut card: Value) -> Result<AgentCard, A2aError> {
    restore_empty_scopes(&mut card);
    let card: AgentCard = serde_json::from_value(card)
        .map_err(|_| A2aError::InvalidCard("it is not a JSON Agent Card"))?;
    if card.name.trim().is_empty() {
        return Err(A2aError::InvalidCard("it has no name"));
    }
    Ok(card)
}

/// The card's own requirements and every skill's.
fn restore_empty_scopes(card: &mut Value) {
    restore_in(card.get_mut("securityRequirements"));
    if let Some(Value::Array(skills)) = card.get_mut("skills") {
        for skill in skills {
            restore_in(skill.get_mut("securityRequirements"));
        }
    }
}

fn restore_in(requirements: Option<&mut Value>) {
    let Some(Value::Array(requirements)) = requirements else {
        return;
    };
    for requirement in requirements {
        let Some(Value::Object(schemes)) = requirement.get_mut("schemes") else {
            continue;
        };
        for scopes in schemes.values_mut() {
            if let Value::Object(wrapped) = scopes {
                wrapped
                    .entry("list")
                    .or_insert_with(|| Value::Array(Vec::new()));
            }
        }
    }
}

#[cfg(test)]
mod tests;

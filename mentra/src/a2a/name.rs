//! The tool name a remote agent is bridged under, and the rule its configured
//! name has to pass first.

/// Prefix applied to a remote agent's name to form its tool name.
const A2A_TOOL_PREFIX: &str = "a2a__";

/// Constructs the tool name a remote agent is bridged under: `a2a__{agent}`.
///
/// `agent_name` must already have passed [`validate_a2a_agent_name`]; this
/// function does not validate it. Every constructor that bridges an agent
/// ([`A2aClient::connect`](super::A2aClient::connect) and through it
/// [`A2aAgentTool`](super::A2aAgentTool)) does.
pub fn a2a_tool_name(agent_name: &str) -> String {
    format!("{A2A_TOOL_PREFIX}{agent_name}")
}

/// Recovers the agent name from a tool name built by [`a2a_tool_name`].
///
/// Returns `None` for a name without the prefix, an empty remainder, or a
/// remainder that is not a valid agent name — so a `Some` is always a name
/// [`a2a_tool_name`] could have produced.
pub fn parse_a2a_tool_name(name: &str) -> Option<&str> {
    let agent = name.strip_prefix(A2A_TOOL_PREFIX)?;
    validate_a2a_agent_name(agent).ok()?;
    Some(agent)
}

/// Errors rejecting a remote agent's configured name.
///
/// The rules are exactly the ones an MCP server name gets
/// ([`validate_mcp_server_name`](crate::mcp::validate_mcp_server_name)), so a
/// host that reads both kinds of configuration applies one rule to both. A
/// bridged A2A tool is a single `a2a__{agent}` today, so no separator inside
/// the name can be misparsed yet; the rule is kept identical so that a
/// later `a2a__{agent}__{skill}` cannot collide, and so that a name valid in
/// one file is valid in the other.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum A2aAgentNameError {
    /// An empty name is never a meaningful identity for a bridged tool.
    #[error("A2A agent name must not be empty")]
    Empty,
    /// A `__` inside the name is the separator the `a2a__` prefix already uses.
    #[error(
        "A2A agent name {0:?} must not contain \"__\", which separates the parts of a \
         bridged tool name"
    )]
    ContainsDoubleUnderscore(String),
    /// A trailing `_` would merge with a `__` separator placed after it.
    #[error(
        "A2A agent name {0:?} must not end with \"_\", which would merge with a \"__\" \
         separator placed after it"
    )]
    EndsWithUnderscore(String),
}

/// Validates a remote agent's configured name before it names a tool.
pub fn validate_a2a_agent_name(agent_name: &str) -> Result<(), A2aAgentNameError> {
    if agent_name.is_empty() {
        return Err(A2aAgentNameError::Empty);
    }
    if agent_name.contains("__") {
        return Err(A2aAgentNameError::ContainsDoubleUnderscore(
            agent_name.to_string(),
        ));
    }
    if agent_name.ends_with('_') {
        return Err(A2aAgentNameError::EndsWithUnderscore(
            agent_name.to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::validate_mcp_server_name;

    #[test]
    fn tool_name_is_prefixed_and_round_trips() {
        assert_eq!(a2a_tool_name("nous"), "a2a__nous");
        assert_eq!(parse_a2a_tool_name("a2a__nous"), Some("nous"));
        assert_eq!(parse_a2a_tool_name("a2a__"), None);
        assert_eq!(parse_a2a_tool_name("mcp__nous__ask"), None);
        assert_eq!(parse_a2a_tool_name("a2a__evil__foo"), None);
    }

    #[test]
    fn names_follow_the_mcp_server_name_rules_exactly() {
        for name in ["", "evil__foo", "evil_", "nous", "a-b", "a_b", "_lead"] {
            assert_eq!(
                validate_a2a_agent_name(name).is_ok(),
                validate_mcp_server_name(name).is_ok(),
                "rules disagree on {name:?}"
            );
        }
        assert_eq!(validate_a2a_agent_name(""), Err(A2aAgentNameError::Empty));
        assert_eq!(
            validate_a2a_agent_name("evil_"),
            Err(A2aAgentNameError::EndsWithUnderscore("evil_".to_string()))
        );
    }
}

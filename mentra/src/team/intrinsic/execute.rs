use crate::{
    ContentBlock,
    agent::Agent,
    error::RuntimeError,
    team::{TeamProtocolStatus, TeamRequestDirection, TeamRequestFilter},
    tool::{ParallelToolContext, ToolCall, ToolResult},
};

use super::schema::{
    TeamBroadcastInput, TeamListRequestsInput, TeamRequestInput, TeamRespondInput, TeamSendInput,
    TeamSpawnInput,
};

pub(super) async fn execute_team_spawn(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamSpawnInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_spawn input: {error}").into(),
                is_error: true,
            };
        }
    };

    match agent
        .spawn_teammate(input.name, input.role, input.prompt)
        .await
    {
        Ok(teammate) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Spawned persistent teammate '{}' (role: {}, status: {})",
                teammate.name, teammate.role, teammate.status
            )
            .into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to spawn teammate: {error}").into(),
            is_error: true,
        },
    }
}

pub(super) fn execute_team_send(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamSendInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_send input: {error}").into(),
                is_error: true,
            };
        }
    };

    match agent.send_team_message(&input.to, input.content) {
        Ok(dispatch) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Sent message to '{}'", dispatch.teammate).into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to send team message: {error}").into(),
            is_error: true,
        },
    }
}

pub(super) fn execute_team_read_inbox(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    match agent.read_team_inbox().and_then(|messages| {
        serde_json::to_string_pretty(&messages).map_err(RuntimeError::FailedToSerializeTeam)
    }) {
        Ok(content) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: content.into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to read team inbox: {error}").into(),
            is_error: true,
        },
    }
}

pub(super) fn execute_team_broadcast(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamBroadcastInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid broadcast input: {error}").into(),
                is_error: true,
            };
        }
    };

    match agent.broadcast_team_message(input.content) {
        Ok(dispatches) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Broadcast message sent to {} recipient(s): {}",
                dispatches.len(),
                dispatches
                    .into_iter()
                    .map(|dispatch| dispatch.teammate)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to broadcast team message: {error}").into(),
            is_error: true,
        },
    }
}

pub(super) fn execute_team_request(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamRequestInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_request input: {error}").into(),
                is_error: true,
            };
        }
    };

    match agent.request_team_protocol(&input.to, input.protocol, input.content) {
        Ok(request) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "Created team request '{}' for '{}' using protocol '{}'",
                request.request_id, request.to, request.protocol
            )
            .into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to create team request: {error}").into(),
            is_error: true,
        },
    }
}

pub(super) fn execute_team_respond(agent: &mut Agent, call: ToolCall) -> ContentBlock {
    let input = match serde_json::from_value::<TeamRespondInput>(call.input) {
        Ok(input) => input,
        Err(error) => {
            return ContentBlock::ToolResult {
                tool_use_id: call.id,
                content: format!("Invalid team_respond input: {error}").into(),
                is_error: true,
            };
        }
    };

    match agent.respond_team_protocol(&input.request_id, input.approve, input.reason) {
        Ok(request) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!(
                "{} team request '{}' ({})",
                if input.approve {
                    "Approved"
                } else {
                    "Rejected"
                },
                request.request_id,
                request.protocol
            )
            .into(),
            is_error: false,
        },
        Err(error) => ContentBlock::ToolResult {
            tool_use_id: call.id,
            content: format!("Failed to respond to team request: {error}").into(),
            is_error: true,
        },
    }
}

pub(super) fn execute_team_list_requests_parallel(
    ctx: ParallelToolContext,
    input: serde_json::Value,
) -> ToolResult {
    let filter = parse_team_request_filter(input)?;
    let config = ctx.runtime.agent_config(&ctx.agent_id)?;
    let requests = ctx
        .runtime
        .list_team_requests(&config.team_dir, &config.name, filter)
        .map_err(|error| format!("Failed to list team requests: {error}"))?;

    serde_json::to_string_pretty(&requests)
        .map_err(|error| format!("Failed to serialize team requests: {error}"))
}

fn parse_team_request_filter(input: serde_json::Value) -> Result<TeamRequestFilter, String> {
    let input = serde_json::from_value::<TeamListRequestsInput>(input)
        .map_err(|error| format!("Invalid team_list_requests input: {error}"))?;

    let status = match input.status.as_deref() {
        Some("pending") => Some(TeamProtocolStatus::Pending),
        Some("approved") => Some(TeamProtocolStatus::Approved),
        Some("rejected") => Some(TeamProtocolStatus::Rejected),
        Some(value) => return Err(format!("Invalid team_list_requests status '{value}'")),
        None => None,
    };

    let direction = match input.direction.as_deref() {
        Some("inbound") => TeamRequestDirection::Inbound,
        Some("outbound") => TeamRequestDirection::Outbound,
        Some("any") | None => TeamRequestDirection::Any,
        Some(value) => return Err(format!("Invalid team_list_requests direction '{value}'")),
    };

    Ok(TeamRequestFilter {
        status,
        protocol: input.protocol,
        counterparty: input.counterparty,
        direction,
    })
}

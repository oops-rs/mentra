mod actor;
mod manager;
mod prompt;
mod store;
mod types;

pub(crate) use actor::teammate_actor_loop;
pub(crate) use manager::TeamManager;
pub(crate) use prompt::{TEAMMATE_MAX_ROUNDS, build_teammate_system_prompt};
pub(crate) use types::format_inbox;
pub use types::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamProtocolRequestSummary,
    TeamProtocolStatus,
};
pub(crate) use types::{TeamRequestDirection, TeamRequestFilter};

pub(crate) const TEAM_SPAWN_TOOL_NAME: &str = "team_spawn";
pub(crate) const TEAM_SEND_TOOL_NAME: &str = "team_send";
pub(crate) const TEAM_READ_INBOX_TOOL_NAME: &str = "team_read_inbox";
pub(crate) const TEAM_BROADCAST_TOOL_NAME: &str = "broadcast";
pub(crate) const TEAM_REQUEST_TOOL_NAME: &str = "team_request";
pub(crate) const TEAM_RESPOND_TOOL_NAME: &str = "team_respond";
pub(crate) const TEAM_LIST_REQUESTS_TOOL_NAME: &str = "team_list_requests";

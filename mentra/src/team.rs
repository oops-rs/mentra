mod actor;
mod host;
mod intrinsic;
mod manager;
mod observer;
mod prompt;
mod store;
mod types;

pub(crate) use actor::teammate_actor_loop;
pub(crate) use host::{TeammateActorHandle, TeammateHost};
pub(crate) use manager::TeamManager;
pub(crate) use observer::{TeamObserverSink, TeamRegistration};
pub(crate) use prompt::{TEAMMATE_MAX_ROUNDS, build_teammate_system_prompt};
pub(crate) use store::TeamStore;
pub(crate) use types::format_inbox;
pub use types::{
    TeamDispatch, TeamMemberStatus, TeamMemberSummary, TeamMessage, TeamMessageKind,
    TeamProtocolRequestSummary, TeamProtocolStatus,
};
pub(crate) use types::{TeamRequestDirection, TeamRequestFilter};

pub(crate) use intrinsic::TeamIntrinsicTool;

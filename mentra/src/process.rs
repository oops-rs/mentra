//! Running another program on a budget, confined the way mentra confines its
//! own commands.
//!
//! A host embedding mentra runs programs of its own — a workspace hook, a
//! formatter, a declared tool that speaks JSON over stdin. Before this module
//! the runtime's process discipline was reachable only by going through
//! [`RuntimeExecutor`](crate::runtime::RuntimeExecutor), whose vocabulary is a
//! shell string with no stdin, so a host that needed a payload or an argv
//! vector wrote its own spawn — and a hand-rolled spawn is where `env_clear`,
//! the process group, the output ceiling and `kill_on_drop` go missing one at a
//! time.
//!
//! [`BoundedCommand`](crate::process::BoundedCommand) is that discipline as a primitive, and the shell executor
//! is a user of it rather than a second copy: there is one implementation of
//! the spawn, the group kill and the capped read in this crate.

mod bounded;
mod capture;

pub use bounded::{BoundedCommand, Completion};
pub use capture::CapturedStream;

use std::borrow::Cow;

pub(crate) const TEAMMATE_MAX_ROUNDS: usize = 50;
const TEAMMATE_SYSTEM_PROMPT: &str = "You are a persistent teammate inside a larger agent team. You may receive new mailbox messages across multiple turns. Use team_send for targeted coordination, broadcast when the same update should go to the whole team, team_request to start structured request-response protocols, team_respond to answer protocol requests, and team_list_requests to inspect approval state when needed. For risky or destructive work, submit a plan_approval request and wait for the matching response before proceeding. If you receive a shutdown request and decide to approve it, send team_respond, finish your current turn cleanly, and then exit. Finish each turn with a concise progress update.";

pub(crate) fn build_teammate_system_prompt(
    base: Option<Cow<'_, str>>,
    name: &str,
    role: &str,
    lead: &str,
) -> String {
    let addition = format!(
        "You are teammate '{name}' with role '{role}' on a team led by '{lead}'. {TEAMMATE_SYSTEM_PROMPT}"
    );
    match base {
        Some(system) => format!("{system}\n\n{addition}"),
        None => addition,
    }
}

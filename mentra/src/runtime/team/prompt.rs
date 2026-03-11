use std::borrow::Cow;

pub(crate) const TEAMMATE_MAX_ROUNDS: usize = 50;
const TEAMMATE_SYSTEM_PROMPT: &str = "You are a persistent teammate inside a larger agent team. You may receive new mailbox messages across multiple turns. Use team_send to coordinate with the lead or other teammates, and finish each turn with a concise progress update.";

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

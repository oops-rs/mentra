use std::{io::Write, path::PathBuf};

use dotenvy::dotenv;
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, ProviderDescriptor,
    runtime::{
        Agent, AgentConfig, AgentEvent, ContextCompactionConfig, Runtime, RuntimePolicy, TaskItem,
        TaskStatus, TeamAutonomyConfig,
    },
    tool::ToolCall,
};
use time::format_description::well_known::Rfc3339;

#[tokio::main]
async fn main() {
    dotenv().ok();

    let runtime = Runtime::builder()
        .with_optional_provider(
            BuiltinProvider::OpenAI,
            std::env::var("OPENAI_API_KEY").ok(),
        )
        .with_optional_provider(
            BuiltinProvider::Anthropic,
            std::env::var("ANTHROPIC_API_KEY").ok(),
        )
        .with_optional_provider(
            BuiltinProvider::Gemini,
            std::env::var("GEMINI_API_KEY").ok(),
        )
        .with_policy(
            RuntimePolicy::default()
                .allow_shell_commands(true)
                .allow_background_commands(true),
        )
        .with_skills_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills"))
        .expect("Failed to register example skills")
        .build()
        .expect("Failed to build runtime");

    let mut agent = runtime
        .spawn_with_config(
            "Foo",
            pick_model(&runtime).await,
            AgentConfig {
                system: Some(
                    "You are a helpful CLI agent. When the user asks to spawn, manage, monitor, or keep working with a named persistent teammate across turns, you must use `team_spawn` and the team protocol tools rather than the disposable `task` tool or persisted task tools. Autonomous teammates can scan persisted tasks, claim ready unowned work themselves, and continue from the task board without manual assignment when team autonomy is enabled. Do not satisfy teammate-management requests by creating project tasks. For plan review workflows, send the teammate a normal message asking for the proposal, let the teammate submit a `plan_approval` request back to you, then use `team_respond` on that inbound request. Do not open a `plan_approval` request to the teammate yourself. Use `task` only for short-lived disposable delegation that does not need mailbox coordination, protocol review, or ongoing status tracking."
                        .to_string(),
                ),
                context_compaction: example_compaction_config(),
                team: mentra::runtime::TeamConfig {
                    autonomy: TeamAutonomyConfig {
                        enabled: true,
                        ..TeamAutonomyConfig::default()
                    },
                    ..mentra::runtime::TeamConfig::default()
                },
                ..AgentConfig::default()
            },
        )
        .expect("Failed to spawn agent");

    let _cli_watcher = subscribe_events(&agent);

    loop {
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .expect("Failed to read line");
        let input = input.trim();
        if input == "exit" {
            break;
        }

        agent
            .send(vec![ContentBlock::Text {
                text: input.to_string(),
            }])
            .await
            .expect("Failed to send message");
    }
}

fn example_compaction_config() -> ContextCompactionConfig {
    let threshold = std::env::var("MENTRA_CHAT_AUTO_COMPACT_TOKENS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3_000);

    ContextCompactionConfig {
        auto_compact_threshold_tokens: (threshold > 0).then_some(threshold),
        ..ContextCompactionConfig::default()
    }
}

async fn pick_model(runtime: &Runtime) -> ModelInfo {
    let providers = runtime.providers();
    let provider = pick_provider(&providers);
    let provider_name = provider_name(&provider);
    let mut discovered_models = match runtime.list_models(Some(&provider.id)).await {
        Ok(models) => models,
        Err(error) => {
            println!("Failed to list models for provider {provider_name}: {error}");
            return prompt_manual_model(&provider);
        }
    };

    discovered_models.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    discovered_models.truncate(10);

    if discovered_models.is_empty() {
        println!("No models were returned for provider {provider_name}.");
        return prompt_manual_model(&provider);
    }

    println!("Available models for {provider_name} (newest to oldest):");
    for (index, model) in discovered_models.iter().enumerate() {
        let display_name = model.display_name.as_deref().unwrap_or(&model.id);
        println!("  {}. {}", index + 1, display_name);

        if display_name != model.id {
            println!("     id: {}", model.id);
        }

        if let Some(created_at) = model.created_at {
            let created_at = created_at
                .format(&Rfc3339)
                .unwrap_or_else(|_| created_at.unix_timestamp().to_string());
            println!("     created_at: {}", created_at);
        }
    }

    loop {
        print!("Pick a model by number: ");
        std::io::stdout().flush().expect("Failed to flush stdout");

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .expect("Failed to read line");

        let selection = input.trim().parse::<usize>();
        match selection {
            Ok(index) if (1..=discovered_models.len()).contains(&index) => {
                let model = discovered_models[index - 1].clone();
                println!("Picked model: {} ({provider_name})", model.id);
                return model;
            }
            _ => {
                println!(
                    "Please enter a number between 1 and {}.",
                    discovered_models.len()
                );
            }
        }
    }
}

fn prompt_manual_model(provider: &ProviderDescriptor) -> ModelInfo {
    println!("Enter a model ID manually for {}.", provider_name(provider));

    loop {
        print!("Model ID: ");
        std::io::stdout().flush().expect("Failed to flush stdout");

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .expect("Failed to read line");

        let model_id = input.trim();
        if model_id.is_empty() {
            println!("Model ID cannot be empty.");
            continue;
        }

        return ModelInfo {
            id: model_id.to_string(),
            provider: provider.id.clone(),
            display_name: None,
            description: Some("Entered manually".to_string()),
            created_at: None,
        };
    }
}

fn pick_provider(providers: &[ProviderDescriptor]) -> ProviderDescriptor {
    if providers.len() == 1 {
        let provider = providers[0].clone();
        println!("Using provider: {}", provider_name(&provider));
        return provider;
    }

    println!("Available providers:");
    for (index, provider) in providers.iter().enumerate() {
        println!("  {}. {}", index + 1, provider_name(provider));
    }

    loop {
        print!("Pick a provider by number: ");
        std::io::stdout().flush().expect("Failed to flush stdout");

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .expect("Failed to read line");

        let selection = input.trim().parse::<usize>();
        match selection {
            Ok(index) if (1..=providers.len()).contains(&index) => {
                let provider = providers[index - 1].clone();
                println!("Using provider: {}", provider_name(&provider));
                return provider;
            }
            _ => {
                println!("Please enter a number between 1 and {}.", providers.len());
            }
        }
    }
}

fn provider_name(provider: &ProviderDescriptor) -> &str {
    provider
        .display_name
        .as_deref()
        .unwrap_or(provider.id.as_str())
}

fn subscribe_events(agent: &Agent) -> tokio::task::JoinHandle<()> {
    let mut events = agent.subscribe_events();
    let mut snapshot = agent.watch_snapshot();

    tokio::spawn(async move {
        let mut assistant_line_open = false;
        let mut last_rendered_tasks = render_tasks(&snapshot.borrow().tasks);
        let mut last_rendered_team_inbox =
            render_team_inbox(snapshot.borrow().pending_team_messages);
        let mut last_rendered_teammates = render_teammates(&snapshot.borrow().teammates);
        let mut last_rendered_protocols =
            render_protocol_requests(&snapshot.borrow().protocol_requests);

        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(AgentEvent::TextDelta { delta, .. }) => {
                        assistant_line_open = true;
                        print!("{delta}");
                        std::io::stdout().flush().expect("Failed to flush stdout");
                    }
                    Ok(AgentEvent::ToolUseReady { call, .. }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!("\x1b[33m$ {}\x1b[0m", describe_tool_call(&call));
                    }
                    Ok(AgentEvent::SubagentSpawned { agent }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[35mspawned subagent\x1b[0m {} ({})",
                            agent.name, agent.id
                        );
                    }
                    Ok(AgentEvent::SubagentFinished { agent }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[35mfinished subagent\x1b[0m {} ({:?})",
                            agent.name, agent.status
                        );
                    }
                    Ok(AgentEvent::TeammateSpawned { teammate }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[32mspawned teammate\x1b[0m {} ({}, {:?})",
                            teammate.name, teammate.role, teammate.status
                        );
                    }
                    Ok(AgentEvent::TeammateUpdated { teammate }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[32mteammate updated\x1b[0m {} ({:?})",
                            teammate.name, teammate.status
                        );
                    }
                    Ok(AgentEvent::BackgroundTaskStarted { task }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[34mstarted background task\x1b[0m {} {}",
                            task.id, task.command
                        );
                    }
                    Ok(AgentEvent::BackgroundTaskFinished { task }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[34mfinished background task\x1b[0m {} ({}) {}",
                            task.id,
                            task.status.as_str(),
                            task.output_preview.as_deref().unwrap_or("(no output)")
                        );
                    }
                    Ok(AgentEvent::ContextCompacted { details }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[36mcontext compacted\x1b[0m {:?} -> {}",
                            details.trigger,
                            details.transcript_path.display()
                        );
                    }
                    Ok(AgentEvent::TeamProtocolRequested { request }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[32mteam request\x1b[0m {} {} -> {} ({})",
                            request.request_id, request.from, request.to, request.protocol
                        );
                    }
                    Ok(AgentEvent::TeamProtocolResolved { request }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[32mteam response\x1b[0m {} {} ({:?})",
                            request.request_id, request.protocol, request.status
                        );
                    }
                    Ok(AgentEvent::TeamInboxUpdated { unread_count }) => {
                        end_assistant_line(&mut assistant_line_open);
                        println!(
                            "\x1b[32mteam inbox updated\x1b[0m {} unread",
                            unread_count
                        );
                    }
                    Ok(AgentEvent::RunFinished) => {
                        end_assistant_line(&mut assistant_line_open);
                    }
                    Ok(AgentEvent::RunFailed { error }) => {
                        end_assistant_line(&mut assistant_line_open);
                        eprintln!("Agent failed: {error}");
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                changed = snapshot.changed() => {
                    if changed.is_err() {
                        break;
                    }

                    let rendered_tasks = render_tasks(&snapshot.borrow().tasks);
                    if rendered_tasks != last_rendered_tasks {
                        end_assistant_line(&mut assistant_line_open);
                        print_tasks(&rendered_tasks);
                        last_rendered_tasks = rendered_tasks;
                    }

                    let rendered_team_inbox =
                        render_team_inbox(snapshot.borrow().pending_team_messages);
                    if rendered_team_inbox != last_rendered_team_inbox {
                        end_assistant_line(&mut assistant_line_open);
                        print_team_inbox(&rendered_team_inbox);
                        last_rendered_team_inbox = rendered_team_inbox;
                    }

                    let rendered_teammates = render_teammates(&snapshot.borrow().teammates);
                    if rendered_teammates != last_rendered_teammates {
                        end_assistant_line(&mut assistant_line_open);
                        print_teammates(&rendered_teammates);
                        last_rendered_teammates = rendered_teammates;
                    }

                    let rendered_protocols = render_protocol_requests(&snapshot.borrow().protocol_requests);
                    if rendered_protocols != last_rendered_protocols {
                        end_assistant_line(&mut assistant_line_open);
                        print_protocol_requests(&rendered_protocols);
                        last_rendered_protocols = rendered_protocols;
                    }
                }
            }
        }
    })
}

fn describe_tool_call(call: &ToolCall) -> String {
    if call.name == "bash"
        && let Some(command) = call.input.get("command").and_then(|value| value.as_str())
    {
        return command.to_string();
    }

    if call.name == "background_run"
        && let Some(command) = call.input.get("command").and_then(|value| value.as_str())
    {
        return format!("background_run {}", command);
    }

    if call.name == "check_background" {
        if let Some(task_id) = call.input.get("task_id").and_then(|value| value.as_str()) {
            return format!("check_background {task_id}");
        }

        return "check_background".to_string();
    }

    if call.name == "read_file"
        && let Some(path) = call.input.get("path").and_then(|value| value.as_str())
    {
        if let Some(lines) = call.input.get("lines").and_then(|value| value.as_u64()) {
            return format!("read_file {} ({lines} lines)", path);
        }

        return format!("read_file {} (all lines)", path);
    }

    if call.name == "task"
        && let Some(prompt) = call.input.get("prompt").and_then(|value| value.as_str())
    {
        return format!("task \"{prompt}\"");
    }

    if call.name == "team_spawn" {
        let name = call
            .input
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        let role = call
            .input
            .get("role")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        return format!("team_spawn {} ({})", name, role);
    }

    if call.name == "team_send"
        && let Some(to) = call.input.get("to").and_then(|value| value.as_str())
    {
        return format!("team_send {}", to);
    }

    if call.name == "team_request"
        && let Some(to) = call.input.get("to").and_then(|value| value.as_str())
        && let Some(protocol) = call.input.get("protocol").and_then(|value| value.as_str())
    {
        return format!("team_request {} ({})", to, protocol);
    }

    if call.name == "team_respond"
        && let Some(request_id) = call
            .input
            .get("request_id")
            .and_then(|value| value.as_str())
    {
        return format!("team_respond {}", request_id);
    }

    if call.name == "team_list_requests" {
        return "team_list_requests".to_string();
    }

    if call.name == "team_read_inbox" {
        return "team_read_inbox".to_string();
    }

    if call.name == "broadcast"
        && let Some(content) = call.input.get("content").and_then(|value| value.as_str())
    {
        return format!("broadcast \"{content}\"");
    }

    if call.name == "task_create"
        && let Some(subject) = call.input.get("subject").and_then(|value| value.as_str())
    {
        return format!("task_create \"{subject}\"");
    }

    if call.name == "task_update"
        && let Some(task_id) = call.input.get("taskId").and_then(|value| value.as_u64())
    {
        return format!("task_update {task_id}");
    }

    if call.name == "task_claim" {
        if let Some(task_id) = call.input.get("taskId").and_then(|value| value.as_u64()) {
            return format!("task_claim {task_id}");
        }

        return "task_claim".to_string();
    }

    if call.name == "task_get"
        && let Some(task_id) = call.input.get("taskId").and_then(|value| value.as_u64())
    {
        return format!("task_get {task_id}");
    }

    if call.name == "task_list" {
        return "task_list".to_string();
    }

    if call.name == "load_skill"
        && let Some(name) = call.input.get("name").and_then(|value| value.as_str())
    {
        return format!("load_skill {name}");
    }

    format!("{} {}", call.name, call.input)
}

fn end_assistant_line(assistant_line_open: &mut bool) {
    if *assistant_line_open {
        println!();
        *assistant_line_open = false;
    }
}

fn print_tasks(rendered_tasks: &str) {
    if rendered_tasks.is_empty() {
        return;
    }

    println!("\x1b[36mTasks\x1b[0m");
    println!("{rendered_tasks}");
}

fn print_team_inbox(rendered_team_inbox: &str) {
    if rendered_team_inbox.is_empty() {
        return;
    }

    println!("\x1b[32mTeam Inbox\x1b[0m");
    println!("{rendered_team_inbox}");
}

fn print_teammates(rendered_teammates: &str) {
    if rendered_teammates.is_empty() {
        return;
    }

    println!("\x1b[32mTeammates\x1b[0m");
    println!("{rendered_teammates}");
}

fn print_protocol_requests(rendered_protocols: &str) {
    if rendered_protocols.is_empty() {
        return;
    }

    println!("\x1b[32mTeam Protocols\x1b[0m");
    println!("{rendered_protocols}");
}

fn render_team_inbox(pending_team_messages: usize) -> String {
    if pending_team_messages == 0 {
        return String::new();
    }

    format!("{pending_team_messages} unread teammate message(s)")
}

fn render_tasks(tasks: &[TaskItem]) -> String {
    if tasks.is_empty() {
        return String::new();
    }

    let mut ready = Vec::new();
    let mut blocked = Vec::new();
    let mut in_progress = Vec::new();
    let mut completed = Vec::new();

    for task in tasks {
        let owner_suffix = if task.owner.trim().is_empty() {
            String::new()
        } else {
            format!(" @{}", task.owner)
        };
        let line = match task.status {
            TaskStatus::Pending if task.blocked_by.is_empty() => {
                format!("[ ] {}: {}{}", task.id, task.subject, owner_suffix)
            }
            TaskStatus::Pending => format!(
                "[-] {}: {}{} (blocked by {})",
                task.id,
                task.subject,
                owner_suffix,
                task.blocked_by
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TaskStatus::InProgress => format!("[>] {}: {}{}", task.id, task.subject, owner_suffix),
            TaskStatus::Completed => format!("[x] {}: {}{}", task.id, task.subject, owner_suffix),
        };

        match task.status {
            TaskStatus::Pending if task.blocked_by.is_empty() => ready.push(line),
            TaskStatus::Pending => blocked.push(line),
            TaskStatus::InProgress => in_progress.push(line),
            TaskStatus::Completed => completed.push(line),
        }
    }

    let sections = [
        ("Ready", ready),
        ("In Progress", in_progress),
        ("Blocked", blocked),
        ("Completed", completed),
    ];

    sections
        .into_iter()
        .filter_map(|(label, items)| {
            if items.is_empty() {
                None
            } else {
                Some(format!("{label}\n{}", items.join("\n")))
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_teammates(teammates: &[mentra::runtime::TeamMemberSummary]) -> String {
    if teammates.is_empty() {
        return String::new();
    }

    teammates
        .iter()
        .map(|teammate| {
            format!("{} ({}, {})", teammate.name, teammate.role, teammate.model)
                + &format!(" - {:?}", teammate.status)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_protocol_requests(requests: &[mentra::runtime::TeamProtocolRequestSummary]) -> String {
    if requests.is_empty() {
        return String::new();
    }

    requests
        .iter()
        .map(|request| {
            let resolution = request
                .resolution_reason
                .as_deref()
                .map(|reason| format!(" - {reason}"))
                .unwrap_or_default();
            format!(
                "[{:?}] {} {} -> {} ({}){}",
                request.status,
                request.request_id,
                request.from,
                request.to,
                request.protocol,
                resolution
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

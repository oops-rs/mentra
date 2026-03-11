use std::io::Write;

use dotenvy::dotenv;
use mentra::{
    provider::model::{ContentBlock, ModelInfo, ModelProviderKind},
    runtime::{Agent, AgentConfig, AgentEvent, Runtime, TodoItem, TodoStatus},
    tool::ToolCall,
};

#[tokio::main]
async fn main() {
    dotenv().ok();

    let mut runtime = Runtime::default();

    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        runtime.register_provider(ModelProviderKind::OpenAI, api_key);
    }

    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        runtime.register_provider(ModelProviderKind::Anthropic, api_key);
    }

    let mut agent = runtime
        .spawn_with_config(
            "Foo",
            pick_model(&runtime).await,
            AgentConfig {
                system: Some("You are a helpful CLI agent.".to_string()),
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

async fn pick_model(runtime: &Runtime) -> ModelInfo {
    let mut discovered_models = Vec::new();

    for provider in runtime.providers() {
        let models = runtime
            .list_models(Some(provider))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{}",
                    format!("Failed to list models for provider {provider}").to_string()
                )
            });

        discovered_models.extend(models);
    }

    assert!(
        !discovered_models.is_empty(),
        "No models found for configured providers"
    );

    println!("Available models:");
    for (index, model) in discovered_models.iter().enumerate() {
        let display_name = model.display_name.as_deref().unwrap_or(&model.id);
        println!(
            "  {}. {} [{}]",
            index + 1,
            display_name,
            provider_name(model.provider)
        );

        if display_name != model.id {
            println!("     id: {}", model.id);
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
                println!(
                    "Picked model: {} ({})",
                    model.id,
                    provider_name(model.provider)
                );
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

fn provider_name(provider: ModelProviderKind) -> &'static str {
    match provider {
        ModelProviderKind::Anthropic => "Anthropic",
        ModelProviderKind::OpenAI => "OpenAI",
        ModelProviderKind::Gemini => "Gemini",
    }
}

fn subscribe_events(agent: &Agent) -> tokio::task::JoinHandle<()> {
    let mut events = agent.subscribe_events();
    let mut snapshot = agent.watch_snapshot();

    tokio::spawn(async move {
        let mut assistant_line_open = false;
        let mut last_rendered_todos = render_todos(&snapshot.borrow().todos);

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

                    let rendered_todos = render_todos(&snapshot.borrow().todos);
                    if rendered_todos != last_rendered_todos {
                        end_assistant_line(&mut assistant_line_open);
                        print_todos(&rendered_todos);
                        last_rendered_todos = rendered_todos;
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

    if call.name == "read_file"
        && let Some(path) = call.input.get("path").and_then(|value| value.as_str())
    {
        if let Some(lines) = call.input.get("lines").and_then(|value| value.as_u64()) {
            return format!("read_file {} ({lines} lines)", path);
        }

        return format!("read_file {} (all lines)", path);
    }

    if call.name == "todo"
        && let Some(items) = call.input.get("items").and_then(|value| value.as_array())
    {
        return format!("todo {} item(s)", items.len());
    }

    if call.name == "task"
        && let Some(prompt) = call.input.get("prompt").and_then(|value| value.as_str())
    {
        return format!("task \"{prompt}\"");
    }

    format!("{} {}", call.name, call.input)
}

fn end_assistant_line(assistant_line_open: &mut bool) {
    if *assistant_line_open {
        println!();
        *assistant_line_open = false;
    }
}

fn print_todos(rendered_todos: &str) {
    if rendered_todos.is_empty() {
        return;
    }

    println!("\x1b[36mTodos\x1b[0m");
    println!("{rendered_todos}");
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return String::new();
    }

    todos
        .iter()
        .map(|item| {
            let marker = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[>]",
                TodoStatus::Completed => "[x]",
            };
            format!("{marker} {}: {}", item.id, item.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

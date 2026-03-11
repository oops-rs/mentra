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

    let mut configured_providers = Vec::new();

    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        runtime.register_provider(ModelProviderKind::OpenAI, api_key);
        configured_providers.push(ModelProviderKind::OpenAI);
    }

    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        runtime.register_provider(ModelProviderKind::Anthropic, api_key);
        configured_providers.push(ModelProviderKind::Anthropic);
    }

    assert!(
        !configured_providers.is_empty(),
        "Set OPENAI_API_KEY or ANTHROPIC_API_KEY before running this example"
    );

    let mut agent = runtime
        .spawn_with_config(
            "Foo",
            pick_model(&runtime, &configured_providers).await,
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

async fn pick_model(runtime: &Runtime, providers: &[ModelProviderKind]) -> ModelInfo {
    for provider in providers {
        let models = runtime
            .list_models(Some(*provider))
            .await
            .expect("Failed to list models");

        if let Some(model) = models.first() {
            println!("Picked model: {:?}", model);
            return model.clone();
        }
    }

    panic!("No models found for configured providers");
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

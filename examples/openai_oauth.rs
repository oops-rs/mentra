use std::io::{self, Read, Write};

use dotenvy::dotenv;
use mentra::{
    Agent, ContentBlock, ModelInfo, ModelSelector, Runtime, agent::AgentEvent,
    auth::openai::{
        OpenAIOAuthClient, OpenAIOAuthCredentialSource, PersistentTokenStoreKind,
        selected_store_kind,
    },
    provider::openai::OpenAIProvider,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let prompt = read_prompt()?;
    let store_kind = selected_store_kind(PersistentTokenStoreKind::Auto);
    let client = OpenAIOAuthClient::default();
    eprintln!("Using OAuth token store backend: {}", store_kind.label());

    let credential_source = OpenAIOAuthCredentialSource::from_persistent_store_or_authorize(
        client,
        PersistentTokenStoreKind::Auto,
        |pending| {
            eprintln!("Open this URL in your browser to authorize Mentra:");
            eprintln!("{}", pending.authorize_url());
            eprintln!();
            eprintln!("Waiting for the callback on {} ...", pending.redirect_uri());
        },
    )
    .await?;

    let runtime = Runtime::builder()
        .with_provider_instance(OpenAIProvider::with_credential_source(credential_source))
        .build()?;

    let model = pick_model(&runtime).await?;
    let mut agent = runtime.spawn("OAuth Quickstart", model)?;
    stream_response(&mut agent, prompt).await?;
    Ok(())
}

fn read_prompt() -> Result<String, Box<dyn std::error::Error>> {
    let from_args = std::env::args().skip(1).collect::<Vec<_>>();
    if !from_args.is_empty() {
        return Ok(from_args.join(" "));
    }

    eprint!("Enter a prompt: ");
    io::stderr().flush()?;

    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let prompt = input.trim().to_string();
    if prompt.is_empty() {
        return Err("Provide a prompt as CLI args or via stdin".into());
    }

    Ok(prompt)
}

async fn pick_model(runtime: &Runtime) -> Result<ModelInfo, Box<dyn std::error::Error>> {
    let selector = std::env::var("MENTRA_MODEL")
        .map(ModelSelector::Id)
        .unwrap_or(ModelSelector::NewestAvailable);

    Ok(runtime
        .resolve_model(mentra::BuiltinProvider::OpenAI, selector)
        .await?)
}

async fn stream_response(
    agent: &mut Agent,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut events = agent.subscribe_events();
    let printer = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                AgentEvent::TextDelta { delta, .. } => {
                    print!("{delta}");
                    let _ = io::stdout().flush();
                }
                AgentEvent::RunFinished => {
                    println!();
                    break;
                }
                AgentEvent::RunFailed { error } => {
                    eprintln!("\nRun failed: {error}");
                    break;
                }
                _ => {}
            }
        }
    });

    agent.send(vec![ContentBlock::text(prompt)]).await?;
    printer.await?;
    Ok(())
}

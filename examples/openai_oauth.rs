use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use dotenvy::dotenv;
use mentra::{
    Agent, ContentBlock, ModelInfo, ModelSelector, Runtime, agent::AgentEvent,
    provider::openai::OpenAIProvider,
};
use mentra_openai_auth::{
    FileTokenStore, OpenAIOAuthClient, OpenAIOAuthCredentialSource, OpenAITokenSet, TokenStore,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let prompt = read_prompt()?;
    let store: Arc<dyn TokenStore> = Arc::new(FileTokenStore::default());
    let client = OpenAIOAuthClient::default();
    let tokens = match store.load()? {
        Some(tokens) => tokens,
        None => authorize(&client, store.as_ref()).await?,
    };

    let runtime = Runtime::builder()
        .with_provider_instance(OpenAIProvider::with_credential_source(
            OpenAIOAuthCredentialSource::new(client, tokens).with_store(store),
        ))
        .build()?;

    let model = pick_model(&runtime).await?;
    let mut agent = runtime.spawn("OAuth Quickstart", model)?;
    stream_response(&mut agent, prompt).await?;
    Ok(())
}

async fn authorize(
    client: &OpenAIOAuthClient,
    store: &dyn TokenStore,
) -> Result<OpenAITokenSet, Box<dyn std::error::Error>> {
    let pending = client.start_authorization().await?;
    eprintln!("Open this URL in your browser to authorize Mentra:");
    eprintln!("{}", pending.authorize_url());
    eprintln!();
    eprintln!("Waiting for the callback on {} ...", pending.redirect_uri());

    let tokens = pending.complete(client).await?;
    store.save(&tokens)?;
    Ok(tokens)
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

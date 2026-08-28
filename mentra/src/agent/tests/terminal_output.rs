//! What a typed turn ([`Agent::run_to_output`]) may do on its way to the
//! answer: the shaping turn that holds one forced tool, the working turn that
//! keeps its whole toolset, and what each does when the terminal call never
//! comes.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AgentConfig, BuiltinProvider, ContentBlock, ModelInfo, Provider, ProviderDescriptor,
    ProviderError, ProviderEventStream, Request, Role, Runtime, TerminalOutputDecision,
    TerminalOutputSpec, TokenUsage,
    error::RuntimeError,
    provider::{Response, ToolChoice},
    provider_event_stream_from_response,
    runtime::{CancellationToken, EarlyEnd, RunOptions},
};

use super::support::{StaticTool, StopTrippingTool};

/// The prefix every generated terminal tool's name carries, which is how the
/// scripted model below finds a tool whose name it cannot know in advance.
const TERMINAL_PREFIX: &str = "mentra_terminal_";

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Review {
    verdict: String,
}

/// One block of a scripted assistant response.
///
/// [`Say::Answer`] is resolved against the request's tool list when the round
/// runs, so a test never has to know the per-call name `run_to_output`
/// generates.
#[derive(Clone)]
enum Say {
    Text(&'static str),
    Call {
        id: &'static str,
        tool: &'static str,
    },
    Answer {
        id: &'static str,
        input: Value,
    },
}

/// One scripted round: what the model says, and what it reports having spent.
#[derive(Clone)]
struct Round {
    blocks: Vec<Say>,
    usage: Option<TokenUsage>,
}

impl Round {
    fn new(blocks: Vec<Say>) -> Self {
        Self {
            blocks,
            usage: None,
        }
    }

    fn spending(mut self, input_tokens: u64, output_tokens: u64) -> Self {
        self.usage = Some(TokenUsage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            ..Default::default()
        });
        self
    }
}

/// What one request put in front of the model — the two things a typed turn
/// changes about a round.
#[derive(Clone, Debug)]
struct Offer {
    tools: Vec<String>,
    choice: Option<ToolChoice>,
}

impl Offer {
    fn terminal_tool(&self) -> Option<&String> {
        self.tools
            .iter()
            .find(|name| name.starts_with(TERMINAL_PREFIX))
    }

    fn ordinary_tools(&self) -> Vec<&String> {
        self.tools
            .iter()
            .filter(|name| !name.starts_with(TERMINAL_PREFIX))
            .collect()
    }
}

/// A model that plays one scripted [`Round`] per request and records what each
/// request offered it.
#[derive(Clone)]
struct ScriptedModel {
    model: ModelInfo,
    rounds: Arc<Mutex<VecDeque<Round>>>,
    offers: Arc<Mutex<Vec<Offer>>>,
}

impl ScriptedModel {
    fn new(rounds: Vec<Round>) -> Self {
        Self {
            model: ModelInfo::new("typed-turn-model", BuiltinProvider::Anthropic),
            rounds: Arc::new(Mutex::new(VecDeque::from(rounds))),
            offers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn offers(&self) -> Vec<Offer> {
        self.offers.lock().expect("offers poisoned").clone()
    }
}

#[async_trait]
impl Provider for ScriptedModel {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let offer = Offer {
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            choice: request.tool_choice.clone(),
        };
        let terminal = offer.terminal_tool().cloned();
        let index = {
            let mut offers = self.offers.lock().expect("offers poisoned");
            offers.push(offer);
            offers.len() - 1
        };
        let round = self
            .rounds
            .lock()
            .expect("rounds poisoned")
            .pop_front()
            .unwrap_or_else(|| panic!("the model was asked for an unscripted round {index}"));

        let mut content = Vec::new();
        for block in round.blocks {
            content.push(match block {
                Say::Text(text) => ContentBlock::text(text),
                Say::Call { id, tool } => ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: tool.to_string(),
                    input: json!({ "value": "please" }),
                },
                Say::Answer { id, input } => ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: terminal
                        .clone()
                        .expect("the terminal tool must be on a typed turn's request"),
                    input,
                },
            });
        }
        let calls_a_tool = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));

        Ok(provider_event_stream_from_response(Response {
            id: format!("message-{index}"),
            model: self.model.id.clone(),
            role: Role::Assistant,
            content,
            stop_reason: calls_a_tool.then(|| "tool_use".to_string()),
            usage: round.usage,
        }))
    }
}

fn review_spec() -> TerminalOutputSpec {
    TerminalOutputSpec::new(
        "submit_review",
        "Return the verdict you reached",
        json!({
            "type": "object",
            "properties": { "verdict": { "type": "string" } },
            "required": ["verdict"]
        }),
    )
}

fn hold() -> Value {
    json!({ "verdict": "hold" })
}

#[tokio::test]
async fn a_reserved_output_rejects_then_accepts_a_transformed_value() {
    let provider = ScriptedModel::new(vec![
        Round::new(vec![Say::Answer {
            id: "draft-1",
            input: json!({ "verdict": "draft" }),
        }]),
        Round::new(vec![Say::Answer {
            id: "answer-1",
            input: hold(),
        }]),
    ]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");
    let reservation = review_spec().reserve();
    let terminal_name = reservation.tool_name().to_string();
    let attempts = Arc::new(Mutex::new(0usize));
    let attempts_for_validator = Arc::clone(&attempts);

    let output = agent
        .run_to_reserved_output::<Review, _>(
            vec![ContentBlock::text("submit a validated review")],
            RunOptions::default(),
            reservation,
            move |candidate| {
                let mut attempts = attempts_for_validator
                    .lock()
                    .expect("attempt counter poisoned");
                *attempts += 1;
                assert_eq!(
                    candidate["verdict"],
                    if *attempts == 1 { "draft" } else { "hold" }
                );
                if *attempts == 1 {
                    TerminalOutputDecision::Reject("draft verdict is not final".to_string())
                } else {
                    TerminalOutputDecision::Accept(json!({ "verdict": "ship" }))
                }
            },
        )
        .await
        .expect("the corrected output is accepted");

    assert_eq!(
        output.value.verdict, "ship",
        "the accepted value is validator-owned"
    );
    assert_eq!(*attempts.lock().expect("attempt counter poisoned"), 2);
    let offers = handle.offers();
    assert_eq!(offers.len(), 2, "a rejection stays inside the same run");
    assert!(
        offers
            .iter()
            .all(|offer| offer.tools.contains(&terminal_name)),
        "the reservation exposes the exact generated tool used by every round"
    );
    assert!(agent.history().iter().any(|message| {
        last_results(message).iter().any(|(id, text, is_error)| {
            id == "draft-1" && *is_error && text.contains("draft verdict is not final")
        })
    }));
}

/// An agent that forces one ordinary tool of its own, so a test can tell a
/// typed turn's choice apart from the default one every agent already sends.
fn forcing_probe() -> AgentConfig {
    AgentConfig {
        tool_choice: Some(ToolChoice::Tool {
            name: "probe".to_string(),
        }),
        ..AgentConfig::default()
    }
}

/// The `(tool_use_id, text, is_error)` of every result on the message that
/// ended the turn, in the order the round committed them.
fn last_results(message: &crate::Message) -> Vec<(String, String, bool)> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.clone(), content.to_display_string(), *is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_shaping_turn_offers_only_the_terminal_tool_and_forces_it() {
    // The default typed turn, unchanged: an agent with a perfectly usable
    // ordinary tool is not offered it, because the turn exists to decide a
    // shape and nothing else.
    let provider = ScriptedModel::new(vec![Round::new(vec![Say::Answer {
        id: "answer-1",
        input: hold(),
    }])]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe", "read the file"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");

    let output = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("shape what you have")],
            RunOptions::default(),
            review_spec(),
        )
        .await
        .expect("a shaping turn answers");

    assert_eq!(output.value.verdict, "hold");
    let offers = handle.offers();
    assert_eq!(offers.len(), 1, "a shaping turn takes one round");
    let terminal = offers[0]
        .terminal_tool()
        .expect("the terminal tool is offered")
        .clone();
    assert_eq!(
        offers[0].tools,
        vec![terminal.clone()],
        "the terminal tool is the only tool on the request"
    );
    assert_eq!(
        offers[0].choice,
        Some(ToolChoice::Tool { name: terminal }),
        "and the model is told to call it"
    );
}

#[tokio::test]
async fn a_working_turn_reaches_an_ordinary_tool_and_then_answers_through_the_terminal_one() {
    // The opt-in: one turn that reads and then answers in the declared shape,
    // where the shaping turn would have needed a turn for each. The agent is
    // configured to force a tool of its own, so the `Auto` below is this
    // turn's doing and not a default.
    let provider = ScriptedModel::new(vec![
        Round::new(vec![Say::Call {
            id: "probe-1",
            tool: "probe",
        }]),
        Round::new(vec![Say::Answer {
            id: "answer-1",
            input: hold(),
        }]),
    ]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe", "read the file"))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("reviewer", model, forcing_probe())
        .expect("spawn agent");

    let output = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("read, then review")],
            RunOptions::default(),
            review_spec().with_tools(),
        )
        .await
        .expect("a working turn answers");

    assert_eq!(output.value.verdict, "hold");
    let offers = handle.offers();
    assert_eq!(offers.len(), 2, "the turn worked a round, then answered");
    for (round, offer) in offers.iter().enumerate() {
        assert!(
            offer.terminal_tool().is_some(),
            "round {round} can end the turn"
        );
        assert_eq!(
            offer.ordinary_tools(),
            vec!["probe"],
            "round {round} keeps the ordinary toolset"
        );
        assert_eq!(
            offer.choice,
            Some(ToolChoice::Auto),
            "round {round} forces nothing: a forced choice — the agent's own \
             included — precludes the working rounds that are the point"
        );
    }

    // The tool really ran — the point of the mode is the reading, not the
    // roster.
    let read_it = agent.history().iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "probe-1" && content.to_display_string() == "read the file")
        })
    });
    assert!(read_it, "the ordinary tool executed: {:?}", agent.history());
}

#[tokio::test]
async fn a_working_turn_that_settles_for_prose_reports_the_missing_terminal_call() {
    // Nothing forces the ending, so a model can work and then simply talk.
    // That is not an answer, and it must not be reported as one.
    let provider = ScriptedModel::new(vec![
        Round::new(vec![Say::Call {
            id: "probe-1",
            tool: "probe",
        }]),
        Round::new(vec![Say::Text("looks fine to me")]),
    ]);
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe", "read the file"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");

    let error = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("read, then review")],
            RunOptions::default(),
            review_spec().with_tools(),
        )
        .await
        .expect_err("prose is not a typed answer");

    assert!(
        error
            .to_string()
            .contains("without invoking the expected terminal tool"),
        "got: {error}"
    );
    assert!(
        agent
            .history()
            .iter()
            .any(|message| message.text().contains("looks fine to me")),
        "the turn keeps what it gathered and said"
    );
}

#[tokio::test]
async fn a_working_turn_stopped_at_a_round_boundary_fails_instead_of_answering_nothing() {
    // A working turn can run many rounds, so a graceful stop can now land
    // between them. It ends the turn exactly as it ends any other — at the
    // boundary, transcript kept — and the typed caller is told the terminal
    // call never came rather than handed a value nobody produced.
    let stop = CancellationToken::default();
    let provider = ScriptedModel::new(vec![
        Round::new(vec![Say::Call {
            id: "probe-1",
            tool: "stop_probe",
        }]),
        Round::new(vec![Say::Answer {
            id: "answer-1",
            input: hold(),
        }]),
    ]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StopTrippingTool::new("stop_probe", stop.clone()))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");

    let options = RunOptions {
        stop: Some(stop),
        ..Default::default()
    };
    let error = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("read, then review")],
            options.clone(),
            review_spec().with_tools(),
        )
        .await
        .expect_err("a turn stopped before the terminal call has no value");

    assert!(
        error
            .to_string()
            .contains("without invoking the expected terminal tool"),
        "got: {error}"
    );
    assert_eq!(
        options.ended_early(),
        Some(EarlyEnd::StopRequested),
        "and the run says which bound ended it"
    );
    assert_eq!(
        handle.offers().len(),
        1,
        "the stop was honored at the boundary: the answering round never ran"
    );
    assert_eq!(
        agent.history().len(),
        3,
        "the gathered round stays committed, not rolled back"
    );
}

#[tokio::test]
async fn a_working_turn_out_of_token_budget_fails_the_same_way() {
    // The other graceful bound, at the same boundary, reported as itself.
    let provider = ScriptedModel::new(vec![
        Round::new(vec![Say::Call {
            id: "probe-1",
            tool: "probe",
        }])
        .spending(60, 40),
        Round::new(vec![Say::Answer {
            id: "answer-1",
            input: hold(),
        }]),
    ]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe", "read the file"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");

    let options = RunOptions {
        token_budget: Some(100),
        ..Default::default()
    };
    let error = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("read, then review")],
            options.clone(),
            review_spec().with_tools(),
        )
        .await
        .expect_err("a turn out of budget before the terminal call has no value");

    assert!(
        error
            .to_string()
            .contains("without invoking the expected terminal tool"),
        "got: {error}"
    );
    assert_eq!(options.ended_early(), Some(EarlyEnd::TokenBudget));
    assert_eq!(
        handle.offers().len(),
        1,
        "the budget halted the run before the answering round"
    );
}

#[tokio::test]
async fn a_terminal_call_beside_other_calls_ends_the_round_and_skips_what_follows() {
    // A working turn is the first typed turn where the model can put other
    // calls in the round it answers from. The terminal tool terminates its
    // round, so calls before it run and calls after it do not — each still
    // getting an explicit result, never a silent drop.
    let provider = ScriptedModel::new(vec![Round::new(vec![
        Say::Call {
            id: "before-1",
            tool: "probe",
        },
        Say::Answer {
            id: "answer-1",
            input: hold(),
        },
        Say::Call {
            id: "after-1",
            tool: "probe",
        },
    ])]);
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe", "read the file"))
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");

    let output = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("read and review in one breath")],
            RunOptions::default(),
            review_spec().with_tools(),
        )
        .await
        .expect("the terminal call in the round is still the answer");

    assert_eq!(output.value.verdict, "hold");
    let results = last_results(&output.message);
    assert_eq!(
        results
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["before-1", "answer-1", "after-1"],
        "every call in the round has exactly one result"
    );
    assert_eq!(
        (results[0].1.as_str(), results[0].2),
        ("read the file", false),
        "the call before the answer ran"
    );
    assert!(
        results[2].1.contains("not executed: run terminated by") && results[2].2,
        "the call after the answer did not run, and says so: {:?}",
        results[2]
    );
}

#[tokio::test]
async fn two_terminal_calls_in_one_round_answer_with_the_first() {
    // The same rule read from the other side: the second terminal call is
    // simply a call scheduled after a terminating one, so the first is the
    // answer and the second is reported as skipped. Deliberate, because a
    // model that emits two shapes has not told anyone which it meant.
    let provider = ScriptedModel::new(vec![Round::new(vec![
        Say::Answer {
            id: "answer-1",
            input: json!({ "verdict": "hold" }),
        },
        Say::Answer {
            id: "answer-2",
            input: json!({ "verdict": "ship" }),
        },
    ])]);
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");

    let output = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("review it")],
            RunOptions::default(),
            review_spec().with_tools(),
        )
        .await
        .expect("the first terminal call answers");

    assert_eq!(output.value.verdict, "hold");
    let results = last_results(&output.message);
    assert_eq!(results.len(), 2);
    assert!(
        results[1].1.contains("not executed: run terminated by") && results[1].2,
        "the second answer was never executed: {:?}",
        results[1]
    );
}

#[tokio::test]
async fn a_working_turn_leaves_the_gate_shut_behind_it() {
    // The gate is per-run: whatever the typed turn did to the roster and to
    // the choice, the next ordinary turn on the same agent is back to its own.
    let provider = ScriptedModel::new(vec![
        Round::new(vec![Say::Answer {
            id: "answer-1",
            input: hold(),
        }]),
        Round::new(vec![Say::Text("back to prose")]),
    ]);
    let handle = provider.clone();
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .with_tool(StaticTool::success("probe", "read the file"))
        .build()
        .expect("build runtime");
    let mut agent = runtime
        .spawn_with_config("reviewer", model, forcing_probe())
        .expect("spawn agent");

    agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("review it")],
            RunOptions::default(),
            review_spec().with_tools(),
        )
        .await
        .expect("a working turn answers");
    let plain = agent
        .send(vec![ContentBlock::text("and now just talk")])
        .await
        .expect("an ordinary turn follows");

    assert_eq!(plain.text(), "back to prose");
    let offers = handle.offers();
    assert_eq!(offers[1].ordinary_tools(), vec!["probe"]);
    assert!(
        offers[1].terminal_tool().is_none(),
        "the generated tool is gone once its run is over: {:?}",
        offers[1]
    );
    assert_eq!(
        offers[1].choice,
        Some(ToolChoice::Tool {
            name: "probe".to_string()
        }),
        "and the agent's own forced choice is back"
    );
}

/// A run that ends with no assistant message and no terminal call is reported
/// as the missing terminal call, not as the empty assistant response
/// `Agent::run` sees. Kept as its own test because it is the one place where
/// the typed helper deliberately reinterprets an error from underneath it.
#[tokio::test]
async fn a_run_that_answers_nothing_at_all_still_names_the_missing_terminal_call() {
    let provider = ScriptedModel::new(vec![Round::new(Vec::new())]);
    let model = provider.model.clone();
    let runtime = Runtime::empty_builder()
        .with_provider_instance(provider)
        .build()
        .expect("build runtime");
    let mut agent = runtime.spawn("reviewer", model).expect("spawn agent");

    let error = agent
        .run_to_output::<Review>(
            vec![ContentBlock::text("review it")],
            RunOptions::default(),
            review_spec(),
        )
        .await
        .expect_err("an empty response is not an answer");

    assert!(
        !matches!(error, RuntimeError::EmptyAssistantResponse),
        "the typed caller asked about the terminal call, not about prose"
    );
    assert!(
        error
            .to_string()
            .contains("without invoking the expected terminal tool"),
        "got: {error}"
    );
}

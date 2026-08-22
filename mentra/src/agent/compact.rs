use crate::memory::journal::CompactionOutcome;
use crate::{
    ContentBlock, Message,
    agent::AgentEvent,
    compaction::compaction_request_from_agent,
    error::{ErrorCategory, RuntimeError},
    memory::{
        estimated_request_tokens, micro_compact_history, required_tail_start_for_continuation,
    },
};

use super::{Agent, CompactionDetails, CompactionTrigger};

const AUTO_COMPACT_MAX_ATTEMPTS: u32 = 3;
const AUTO_COMPACT_RETRY_DELAY_MS: u64 = 500;

impl Agent {
    pub(crate) fn micro_compacted_history(&self) -> Vec<Message> {
        micro_compact_history(
            self.history(),
            self.config.compaction.keep_recent_tool_results,
        )
    }

    pub(crate) fn estimated_request_tokens(&self, messages: &[Message]) -> usize {
        estimated_request_tokens(messages, self.effective_system_prompt().as_deref())
    }

    pub(crate) async fn auto_compact_if_needed(&mut self) -> Result<(), RuntimeError> {
        let Some(threshold) = self
            .config
            .compaction
            .auto_compact_threshold(self.context_window())
        else {
            return Ok(());
        };

        let messages = self.micro_compacted_history();
        if self.estimated_request_tokens(&messages) <= threshold {
            return Ok(());
        }

        let preserve_from = required_tail_start_for_continuation(self.history());

        for attempt in 1..=AUTO_COMPACT_MAX_ATTEMPTS {
            match self
                .compact_history(preserve_from, CompactionTrigger::Auto)
                .await
            {
                Ok(_) => return Ok(()),
                Err(err)
                    if err.category() == ErrorCategory::Retryable
                        && attempt < AUTO_COMPACT_MAX_ATTEMPTS =>
                {
                    self.emit_event(AgentEvent::RetryAttempt {
                        agent_id: self.id().to_string(),
                        error_message: err.to_string(),
                        attempt,
                        max_attempts: AUTO_COMPACT_MAX_ATTEMPTS,
                        next_delay_ms: AUTO_COMPACT_RETRY_DELAY_MS,
                    });
                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        AUTO_COMPACT_RETRY_DELAY_MS,
                    ))
                    .await;
                }
                Err(_) => {
                    // Non-retryable error or all attempts exhausted: degrade gracefully.
                    // The session continues with micro-compaction only.
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    /// Compacts because the provider refused the request as too long.
    ///
    /// Unlike [`auto_compact_if_needed`](Self::auto_compact_if_needed) this
    /// consults no threshold: the threshold is an estimate of what will fit and
    /// the provider has just said, authoritatively, that it did not. There is
    /// nothing left to predict.
    pub(crate) async fn compact_after_context_overflow(&mut self) -> Result<(), RuntimeError> {
        let preserve_from = required_tail_start_for_continuation(self.history());
        self.compact_history(preserve_from, CompactionTrigger::Auto)
            .await?;
        Ok(())
    }

    pub(crate) async fn compact_history(
        &mut self,
        preserve_from: usize,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionDetails>, RuntimeError> {
        self.compact_history_with_instructions(preserve_from, trigger, None)
            .await
    }

    /// Compacts the transcript, telling the summarizer what the caller wants
    /// kept.
    ///
    /// The instructions are added to the standing continuity requirements
    /// rather than replacing them: a caller asking for one extra thing should
    /// not thereby lose the file paths and command outcomes every summary
    /// needs.
    pub(crate) async fn compact_history_with_instructions(
        &mut self,
        preserve_from: usize,
        trigger: CompactionTrigger,
        instructions: Option<&str>,
    ) -> Result<Option<CompactionDetails>, RuntimeError> {
        if self.history().is_empty() {
            return Ok(None);
        }

        // A `preserve_from` of zero used to end the attempt here. Zero means
        // the protected tail is the whole transcript — a single turn that is
        // itself over budget — which is precisely when compaction is most
        // needed. The engine has a split-turn path for it, so let it decide
        // rather than silently doing nothing.
        debug_assert!(preserve_from <= self.history().len());

        let base_revision = self.memory.revision();
        // Compaction is a provider request like any other, so it goes out on
        // the transport the runtime chose. Leaving it on the request's own
        // value would quietly summarize over HTTP+SSE inside a run the host
        // put on a websocket.
        let mut provider_request_options = self.config.provider_request_options.clone();
        crate::provider::select_responses_transport(
            self.provider.as_ref(),
            self.runtime.responses_transport(),
            &mut provider_request_options,
        )?;
        let Some(proposal) = self
            .runtime
            .compaction_engine()
            .compact(self.provider.clone(), {
                let mut request = compaction_request_from_agent(
                    self.model(),
                    self.transcript().clone(),
                    &self.config.compaction,
                    provider_request_options,
                );
                request.instructions = instructions.map(str::to_string);
                request
            })
            .await?
        else {
            return Ok(None);
        };
        let transcript_path = proposal.transcript_path.clone();
        let replaced_items = proposal.replaced_items;
        let preserved_items = proposal.preserved_items;
        let summary = proposal.summary.clone();
        self.runtime
            .emit_hook(crate::runtime::RuntimeHookEvent::MemoryCompactionProposed {
                agent_id: self.id().to_string(),
                base_revision,
                transcript_path: transcript_path.clone(),
            })?;
        let applied = self.memory.try_apply_compaction(
            base_revision,
            CompactionOutcome {
                transcript_path: proposal.transcript_path,
                transcript: proposal.transcript,
            },
        )?;
        if !applied {
            let _ =
                self.runtime
                    .emit_hook(crate::runtime::RuntimeHookEvent::MemoryCompactionSkipped {
                        agent_id: self.id().to_string(),
                        base_revision,
                    });
            return Ok(None);
        }
        self.runtime.memory_engine().store_compaction_summary(
            self.id(),
            self.memory.revision(),
            &summary.render_for_handoff(),
        )?;
        self.sync_memory_snapshot();
        let _ = self
            .runtime
            .emit_hook(crate::runtime::RuntimeHookEvent::MemoryCompactionApplied {
                agent_id: self.id().to_string(),
                base_revision,
                resulting_history_len: self.transcript().len(),
            });

        let details = CompactionDetails {
            trigger,
            mode: proposal.mode,
            agent_id: self.id().to_string(),
            transcript_path,
            replaced_items,
            preserved_items,
            preserved_user_turns: proposal.preserved_user_turns,
            preserved_delegation_results: proposal.preserved_delegation_results,
            resulting_transcript_len: self.transcript().len(),
            extracted_facts_count: proposal.diagnostics.extracted_facts_count,
            summary_preview: proposal.diagnostics.summary_preview.clone(),
        };
        self.emit_event(AgentEvent::ContextCompacted {
            details: details.clone(),
        });

        Ok(Some(details))
    }

    pub(crate) fn inject_teammate_identity(&self, messages: &mut Vec<Message>) {
        let Some(identity) = &self.teammate_identity else {
            return;
        };
        if messages.len() > 5 {
            return;
        }

        messages.insert(
            0,
            Message::user(ContentBlock::Text {
                text: format!(
                    "<identity>You are teammate '{}' with role '{}' on the team led by '{}'. Continue your assigned work and stay in character.</identity>",
                    self.name, identity.role, identity.lead
                ),
            }),
        );
        messages.insert(
            1,
            Message::assistant(ContentBlock::Text {
                text: format!("I am {}. Continuing.", self.name),
            }),
        );
    }
}

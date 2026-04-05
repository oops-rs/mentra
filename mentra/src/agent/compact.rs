use crate::memory::journal::CompactionOutcome;
use crate::{
    ContentBlock, Message,
    agent::AgentEvent,
    compaction::compaction_request_from_agent,
    error::RuntimeError,
    memory::{
        estimated_request_tokens, micro_compact_history, required_tail_start_for_continuation,
    },
};

use super::{Agent, CompactionDetails, CompactionTrigger};

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
        let Some(threshold) = self.config.compaction.auto_compact_threshold_tokens else {
            return Ok(());
        };

        let messages = self.micro_compacted_history();
        if self.estimated_request_tokens(&messages) <= threshold {
            return Ok(());
        }

        let preserve_from = required_tail_start_for_continuation(self.history());
        let _ = self
            .compact_history(preserve_from, CompactionTrigger::Auto)
            .await?;
        Ok(())
    }

    pub(crate) async fn compact_history(
        &mut self,
        preserve_from: usize,
        trigger: CompactionTrigger,
    ) -> Result<Option<CompactionDetails>, RuntimeError> {
        if self.history().is_empty() {
            return Ok(None);
        }

        let preserve_from = preserve_from.min(self.history().len());
        let summary_target = &self.transcript().items()[..preserve_from];
        if summary_target.is_empty() {
            return Ok(None);
        }

        let base_revision = self.memory.revision();
        let Some(proposal) = self
            .runtime
            .compaction_engine()
            .compact(
                self.provider.clone(),
                compaction_request_from_agent(
                    self.model(),
                    self.transcript().clone(),
                    &self.config.compaction,
                    self.config.provider_request_options.clone(),
                ),
            )
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

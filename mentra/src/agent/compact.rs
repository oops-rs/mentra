use crate::memory::journal::CompactionOutcome;
use crate::{
    ContentBlock, Message,
    agent::AgentEvent,
    error::RuntimeError,
    memory::{
        estimated_request_tokens, micro_compact_history, required_tail_start_for_continuation,
    },
};

use super::{Agent, ContextCompactionDetails, ContextCompactionTrigger};

impl Agent {
    pub(crate) fn micro_compacted_history(&self) -> Vec<Message> {
        micro_compact_history(
            self.history(),
            self.config.context_compaction.keep_recent_tool_results,
        )
    }

    pub(crate) fn estimated_request_tokens(&self, messages: &[Message]) -> usize {
        estimated_request_tokens(messages, self.effective_system_prompt().as_deref())
    }

    pub(crate) async fn auto_compact_if_needed(&mut self) -> Result<(), RuntimeError> {
        let Some(threshold) = self.config.context_compaction.auto_compact_threshold_tokens else {
            return Ok(());
        };

        let messages = self.micro_compacted_history();
        if self.estimated_request_tokens(&messages) <= threshold {
            return Ok(());
        }

        let preserve_from = required_tail_start_for_continuation(self.history());
        let _ = self
            .compact_history(preserve_from, ContextCompactionTrigger::Auto)
            .await?;
        Ok(())
    }

    pub(crate) async fn compact_history(
        &mut self,
        preserve_from: usize,
        trigger: ContextCompactionTrigger,
    ) -> Result<Option<ContextCompactionDetails>, RuntimeError> {
        if self.history().is_empty() {
            return Ok(None);
        }

        let preserve_from = preserve_from.min(self.history().len());
        let summary_target = &self.history()[..preserve_from];
        if summary_target.is_empty() {
            return Ok(None);
        }

        let base_revision = self.memory.revision();
        let Some(proposal) = self
            .runtime
            .memory_engine()
            .compact(
                self.provider.clone(),
                crate::memory::CompactRequest {
                    agent_id: self.id().to_string(),
                    base_revision,
                    history: self.history().to_vec(),
                    preserve_from,
                    trigger: trigger.clone(),
                    transcript_dir: self.config.context_compaction.transcript_dir.clone(),
                    summary_max_input_chars: self.config.context_compaction.summary_max_input_chars,
                    summary_max_output_tokens: self
                        .config
                        .context_compaction
                        .summary_max_output_tokens,
                    model: self.model.clone(),
                    provider_request_options: self.config.provider_request_options.clone(),
                },
            )
            .await?
        else {
            return Ok(None);
        };
        let transcript_path = proposal.transcript_path.clone();
        let replaced_messages = proposal.replaced_messages;
        let preserved_messages = proposal.preserved_messages;
        let summary = proposal.summary.clone();
        let applied = self.memory.try_apply_compaction(
            proposal.base_revision,
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
            &summary,
        )?;
        self.sync_memory_snapshot();
        let _ = self
            .runtime
            .emit_hook(crate::runtime::RuntimeHookEvent::MemoryCompactionApplied {
                agent_id: self.id().to_string(),
                base_revision,
                resulting_history_len: self.history().len(),
            });

        let details = ContextCompactionDetails {
            trigger,
            transcript_path,
            replaced_messages,
            preserved_messages,
            resulting_history_len: self.history().len(),
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
        if messages.len() > 3 {
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

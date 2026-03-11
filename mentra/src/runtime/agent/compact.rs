use std::{
    borrow::Cow,
    collections::HashMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{ContentBlock, Message, Role, provider::Request, runtime::error::RuntimeError};

use super::{Agent, ContextCompactionDetails, ContextCompactionTrigger};

const MICRO_COMPACT_MIN_CONTENT_LEN: usize = 100;

impl Agent {
    pub(crate) fn micro_compacted_history(&self) -> Vec<Message> {
        let keep_recent = self.config.context_compaction.keep_recent_tool_results;
        if keep_recent == usize::MAX {
            return self.history.clone();
        }

        let mut history = self.history.clone();
        let tool_names = self.tool_name_index();
        let mut tool_results = Vec::new();

        for (message_index, message) in history.iter().enumerate() {
            if message.role != Role::User {
                continue;
            }

            for (block_index, block) in message.content.iter().enumerate() {
                if matches!(block, ContentBlock::ToolResult { .. }) {
                    tool_results.push((message_index, block_index));
                }
            }
        }

        if tool_results.len() <= keep_recent {
            return history;
        }

        let compact_count = tool_results.len() - keep_recent;
        for (message_index, block_index) in tool_results.into_iter().take(compact_count) {
            let Some(ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            }) = history[message_index].content.get_mut(block_index)
            else {
                continue;
            };

            if content.len() <= MICRO_COMPACT_MIN_CONTENT_LEN {
                continue;
            }

            let tool_name = tool_names
                .get(tool_use_id.as_str())
                .map(String::as_str)
                .unwrap_or("tool");
            content.clear();
            content.push_str(&format!("[Previous: used {tool_name}]"));
        }

        history
    }

    pub(crate) fn estimated_request_tokens(&self, messages: &[Message]) -> usize {
        let mut estimated =
            Self::estimated_tokens_for_str(&serde_json::to_string(messages).unwrap_or_default());

        if let Some(system) = self.effective_system_prompt() {
            estimated += Self::estimated_tokens_for_str(system.as_ref());
        }

        estimated
    }

    pub(crate) async fn auto_compact_if_needed(&mut self) -> Result<(), RuntimeError> {
        let Some(threshold) = self.config.context_compaction.auto_compact_threshold_tokens else {
            return Ok(());
        };

        let messages = self.micro_compacted_history();
        if self.estimated_request_tokens(&messages) <= threshold {
            return Ok(());
        }

        let preserve_from = self.required_tail_start_for_continuation();
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
        if self.history.is_empty() {
            return Ok(None);
        }

        let preserve_from = preserve_from.min(self.history.len());
        let summary_target = &self.history[..preserve_from];
        if summary_target.is_empty() {
            return Ok(None);
        }

        let transcript_path = self.persist_transcript().await?;
        let summary = self.summarize_messages(summary_target).await?;
        let replaced_messages = summary_target.len();
        let preserved_messages = self.history.len() - preserve_from;
        let mut next_history = Vec::with_capacity(self.history.len() - preserve_from + 1);
        next_history.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!("[Compressed context]\n\n{summary}"),
            }],
        });
        next_history.extend_from_slice(&self.history[preserve_from..]);
        self.replace_history(next_history);

        let details = ContextCompactionDetails {
            trigger,
            transcript_path,
            replaced_messages,
            preserved_messages,
            resulting_history_len: self.history.len(),
        };
        self.emit_event(crate::runtime::AgentEvent::ContextCompacted {
            details: details.clone(),
        });

        Ok(Some(details))
    }

    fn required_tail_start_for_continuation(&self) -> usize {
        let Some(last_index) = self.history.len().checked_sub(1) else {
            return 0;
        };
        let last_message = &self.history[last_index];

        if last_message.role == Role::User
            && last_message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            && last_index > 0
            && self.history[last_index - 1].role == Role::Assistant
            && self.history[last_index - 1]
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        {
            last_index - 1
        } else {
            last_index
        }
    }

    fn tool_name_index(&self) -> HashMap<String, String> {
        let mut tool_names = HashMap::new();

        for message in &self.history {
            for block in &message.content {
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    tool_names.insert(id.clone(), name.clone());
                }
            }
        }

        tool_names
    }

    async fn persist_transcript(&self) -> Result<PathBuf, RuntimeError> {
        let transcript_dir = &self.config.context_compaction.transcript_dir;
        tokio::fs::create_dir_all(transcript_dir)
            .await
            .map_err(RuntimeError::FailedToPersistTranscript)?;

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let transcript_path = transcript_dir.join(format!("{timestamp}.jsonl"));
        let mut serialized = String::new();

        for message in &self.history {
            let line = serde_json::to_string(message)
                .map_err(RuntimeError::FailedToSerializeTranscript)?;
            serialized.push_str(&line);
            serialized.push('\n');
        }

        tokio::fs::write(&transcript_path, serialized)
            .await
            .map_err(RuntimeError::FailedToPersistTranscript)?;

        Ok(transcript_path)
    }

    async fn summarize_messages(&self, messages: &[Message]) -> Result<String, RuntimeError> {
        let serialized =
            serde_json::to_string(messages).map_err(RuntimeError::FailedToSerializeTranscript)?;
        let transcript = Self::truncate_to_char_boundary(
            &serialized,
            self.config.context_compaction.summary_max_input_chars,
        );
        let system = "You compress agent conversations for continuity. Preserve the user goal, key decisions, relevant code paths, important tool outputs, open questions, and remaining work. Keep it concise and factual.";
        let prompt = format!(
            "Summarize this conversation for continuity. The summary should help a future model continue the work without the full transcript.\n\nTranscript JSON:\n{transcript}"
        );

        let response = self
            .provider
            .send(Request {
                model: self.model.as_str().into(),
                system: Some(Cow::Borrowed(system)),
                messages: Cow::Owned(vec![Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: prompt }],
                }]),
                tools: Cow::Owned(Vec::new()),
                tool_choice: None,
                temperature: Some(0.0),
                max_output_tokens: Some(self.config.context_compaction.summary_max_output_tokens),
                metadata: Cow::Borrowed(&self.config.metadata),
            })
            .await
            .map_err(RuntimeError::FailedToCompactHistory)?;

        let summary = response
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if summary.trim().is_empty() {
            Ok(
                "Earlier conversation was compacted, but the summarizer returned no text."
                    .to_string(),
            )
        } else {
            Ok(summary)
        }
    }

    fn estimated_tokens_for_str(text: &str) -> usize {
        text.chars().count().div_ceil(4)
    }

    fn truncate_to_char_boundary(text: &str, max_chars: usize) -> String {
        text.chars().take(max_chars).collect()
    }
}

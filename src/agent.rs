//! Agent processes: channels, branches, workers, compactor, cortex.

pub mod branch;
pub mod channel;
pub mod channel_attachments;
pub mod channel_dispatch;
pub mod channel_history;
pub mod channel_prompt;
pub mod compactor;
pub mod cortex;
pub mod cortex_chat;
pub mod ingestion;
#[cfg(test)]
mod invariant_harness;
pub mod status;
pub mod worker;

pub(crate) fn extract_last_assistant_text(history: &[rig::message::Message]) -> Option<String> {
    for message in history.iter().rev() {
        if let rig::message::Message::Assistant { content, .. } = message {
            let mut combined = String::new();
            for item in content.iter() {
                if let rig::message::AssistantContent::Text(text) = item {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&text.text);
                }
            }
            if !combined.is_empty() {
                return Some(combined);
            }
        }
    }

    None
}

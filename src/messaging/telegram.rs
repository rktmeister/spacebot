//! Telegram messaging adapter using teloxide.

use crate::config::TelegramPermissions;
use crate::messaging::apply_runtime_adapter_to_conversation_id;
use crate::messaging::traits::{InboundStream, Messaging};
use crate::{Attachment, InboundMessage, MessageContent, OutboundResponse, StatusUpdate};

use anyhow::Context as _;
use arc_swap::ArcSwap;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use serde::Serialize;
use teloxide::payloads::setters::*;
use teloxide::requests::{Request, Requester};
use teloxide::types::MessageEntityRef;
use teloxide::types::{
    ChatAction, ChatId, FileId, InputFile, InputPollOption, MediaKind, MessageEntity,
    MessageEntityKind, MessageId, MessageKind, ReactionType, ReplyParameters, UpdateKind, UserId,
};
use teloxide::{ApiError, Bot, RequestError};

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

#[cfg(test)]
use regex::Regex;
#[cfg(test)]
use std::sync::LazyLock;

/// Maximum number of rejected DM users to remember.
const REJECTED_USERS_CAPACITY: usize = 50;

/// Telegram adapter state.
pub struct TelegramAdapter {
    runtime_key: String,
    permissions: Arc<ArcSwap<TelegramPermissions>>,
    bot: Bot,
    bot_user_id: Arc<RwLock<Option<UserId>>>,
    bot_username: Arc<RwLock<Option<String>>>,
    /// Maps conversation_id to the message_id being edited during streaming.
    active_messages: Arc<RwLock<HashMap<String, ActiveStream>>>,
    /// Repeating typing indicator tasks per conversation_id.
    typing_tasks: Arc<RwLock<HashMap<String, JoinHandle<()>>>>,
    /// Shutdown signal for the polling loop.
    shutdown_tx: Arc<RwLock<Option<mpsc::Sender<()>>>>,
}

/// Tracks an in-progress streaming message edit.
struct ActiveStream {
    chat_id: ChatId,
    message_id: MessageId,
    last_edit: Instant,
}

/// Telegram's per-message character limit.
const MAX_MESSAGE_LENGTH: usize = 4096;

/// Minimum interval between streaming edits to avoid rate limits.
const STREAM_EDIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

impl TelegramAdapter {
    pub fn new(
        runtime_key: impl Into<String>,
        token: impl Into<String>,
        permissions: Arc<ArcSwap<TelegramPermissions>>,
    ) -> Self {
        let runtime_key = runtime_key.into();
        let token = token.into();
        let bot = Bot::new(&token);
        Self {
            runtime_key,
            permissions,
            bot,
            bot_user_id: Arc::new(RwLock::new(None)),
            bot_username: Arc::new(RwLock::new(None)),
            active_messages: Arc::new(RwLock::new(HashMap::new())),
            typing_tasks: Arc::new(RwLock::new(HashMap::new())),
            shutdown_tx: Arc::new(RwLock::new(None)),
        }
    }

    fn extract_chat_id(&self, message: &InboundMessage) -> anyhow::Result<ChatId> {
        let id = message
            .metadata
            .get("telegram_chat_id")
            .and_then(|v| v.as_i64())
            .context("missing telegram_chat_id in metadata")?;
        Ok(ChatId(id))
    }

    fn extract_message_id(&self, message: &InboundMessage) -> anyhow::Result<MessageId> {
        let id = message
            .metadata
            .get("telegram_message_id")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .context("missing telegram_message_id in metadata")?;
        Ok(MessageId(id))
    }

    async fn stop_typing(&self, conversation_id: &str) {
        if let Some(handle) = self.typing_tasks.write().await.remove(conversation_id) {
            handle.abort();
        }
    }
}

impl Messaging for TelegramAdapter {
    fn name(&self) -> &str {
        &self.runtime_key
    }

    async fn start(&self) -> crate::Result<InboundStream> {
        let (inbound_tx, inbound_rx) = mpsc::channel(256);
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        *self.shutdown_tx.write().await = Some(shutdown_tx);

        // Resolve bot identity
        let me = self
            .bot
            .get_me()
            .send()
            .await
            .context("failed to call getMe on Telegram")?;
        *self.bot_user_id.write().await = Some(me.id);
        *self.bot_username.write().await = me.username.clone();
        tracing::info!(
            bot_name = %me.first_name,
            bot_username = ?me.username,
            "telegram connected"
        );

        let bot = self.bot.clone();
        let runtime_key = self.runtime_key.clone();
        let permissions = self.permissions.clone();
        let bot_user_id = self.bot_user_id.clone();
        let bot_username = self.bot_username.clone();

        tokio::spawn(async move {
            let mut offset = 0i32;
            // Track users whose DMs were rejected so we can nudge them when they're allowed.
            let mut rejected_users: VecDeque<(ChatId, i64)> = VecDeque::new();
            // Snapshot the current allow list so we can detect changes.
            let mut last_allowed: Vec<i64> = permissions.load().dm_allowed_users.clone();

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        tracing::info!("telegram polling loop shutting down");
                        break;
                    }
                    result = bot.get_updates().offset(offset).timeout(10).send() => {
                        let updates = match result {
                            Ok(updates) => updates,
                            Err(error) => {
                                tracing::error!(%error, "telegram getUpdates failed");
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                continue;
                            }
                        };

                        // Check if the allow list changed and nudge newly-allowed users.
                        let current_permissions = permissions.load();
                        if current_permissions.dm_allowed_users != last_allowed {
                            let newly_allowed: Vec<i64> = current_permissions.dm_allowed_users.iter()
                                .filter(|id| !last_allowed.contains(id))
                                .copied()
                                .collect();

                            if !newly_allowed.is_empty() {
                                // Notify rejected users who are now allowed.
                                let mut remaining = VecDeque::new();
                                for (chat_id, user_id) in rejected_users.drain(..) {
                                    if newly_allowed.contains(&user_id) {
                                        tracing::info!(
                                            user_id,
                                            "notifying previously rejected user they are now allowed"
                                        );
                                        let _ = bot.send_message(
                                            chat_id,
                                            "You've been added to the allow list — send me a message!",
                                        ).send().await;
                                    } else {
                                        remaining.push_back((chat_id, user_id));
                                    }
                                }
                                rejected_users = remaining;
                            }

                            last_allowed = current_permissions.dm_allowed_users.clone();
                        }

                        for update in updates {
                            offset = update.id.as_offset();

                            let message = match &update.kind {
                                UpdateKind::Message(message) => message,
                                _ => continue,
                            };

                            let bot_id = *bot_user_id.read().await;

                            // Skip our own messages
                            if let Some(from) = &message.from
                                && bot_id.is_some_and(|id| from.id == id) {
                                    continue;
                                }

                            let permissions = permissions.load();

                            let chat_id = message.chat.id.0;
                            let is_private = message.chat.is_private();

                            // DM filter: in private chats, check dm_allowed_users
                            if is_private {
                                if let Some(from) = &message.from
                                    && !permissions.dm_allowed_users.is_empty()
                                        && !permissions
                                            .dm_allowed_users
                                            .contains(&(from.id.0 as i64))
                                    {
                                        // Remember this user so we can nudge them if they're added later.
                                        let entry = (message.chat.id, from.id.0 as i64);
                                        if !rejected_users.iter().any(|(_, uid)| *uid == entry.1) {
                                            if rejected_users.len() >= REJECTED_USERS_CAPACITY {
                                                rejected_users.pop_front();
                                            }
                                            rejected_users.push_back(entry);
                                        }
                                        continue;
                                    }
                            } else if let Some(filter) = &permissions.chat_filter {
                                // Chat filter: if configured, only allow listed group/channel chats
                                if !filter.contains(&chat_id) {
                                    tracing::debug!(
                                        chat_id,
                                        ?filter,
                                        "telegram message rejected by chat filter"
                                    );
                                    continue;
                                }
                            }

                            // Extract text content
                            let text = extract_text(message);
                            if text.is_none() && !has_attachments(message) {
                                continue;
                            }

                            let content = build_content(&bot, message, &text).await;
                            let base_conversation_id = format!("telegram:{chat_id}");
                            let conversation_id = apply_runtime_adapter_to_conversation_id(
                                &runtime_key,
                                base_conversation_id,
                            );
                            let sender_id = message
                                .from
                                .as_ref()
                                .map(|u| u.id.0.to_string())
                                .unwrap_or_default();

                            let (metadata, formatted_author) = build_metadata(
                                message,
                                &*bot_username.read().await,
                            );

                            let inbound = InboundMessage {
                                id: message.id.0.to_string(),
                                source: "telegram".into(),
                                adapter: Some(runtime_key.clone()),
                                conversation_id,
                                sender_id,
                                agent_id: None,
                                content,
                                timestamp: message.date,
                                metadata,
                                formatted_author,
                            };

                            if let Err(error) = inbound_tx.send(inbound).await {
                                tracing::warn!(
                                    %error,
                                    "failed to send inbound message from Telegram (receiver dropped)"
                                );
                                return;
                            }
                        }
                    }
                }
            }
        });

        let stream = tokio_stream::wrappers::ReceiverStream::new(inbound_rx);
        Ok(Box::pin(stream))
    }

    async fn respond(
        &self,
        message: &InboundMessage,
        response: OutboundResponse,
    ) -> crate::Result<()> {
        let chat_id = self.extract_chat_id(message)?;

        match response {
            OutboundResponse::Text(text) => {
                self.stop_typing(&message.conversation_id).await;
                send_formatted(&self.bot, chat_id, &text, None).await?;
            }
            OutboundResponse::RichMessage { text, poll, .. } => {
                self.stop_typing(&message.conversation_id).await;
                send_formatted(&self.bot, chat_id, &text, None).await?;

                if let Some(poll_data) = poll {
                    send_poll(&self.bot, chat_id, &poll_data).await?;
                }
            }
            OutboundResponse::ThreadReply {
                thread_name: _,
                text,
            } => {
                self.stop_typing(&message.conversation_id).await;

                // Telegram doesn't have named threads. Reply to the source message instead.
                let reply_to = self.extract_message_id(message).ok();
                send_formatted(&self.bot, chat_id, &text, reply_to).await?;
            }
            OutboundResponse::File {
                filename,
                data,
                mime_type,
                caption,
            } => {
                self.stop_typing(&message.conversation_id).await;

                // Use send_audio for audio files so Telegram renders an inline player.
                // Fall back to send_document for everything else.
                if mime_type.starts_with("audio/") {
                    let input_file = InputFile::memory(data.clone()).file_name(filename.clone());
                    let sent = if let Some(ref caption_text) = caption {
                        let rendered_caption = markdown_to_telegram_entities(caption_text);
                        self.bot
                            .send_audio(chat_id, input_file)
                            .caption(rendered_caption.text)
                            .caption_entities(rendered_caption.entities)
                            .send()
                            .await
                    } else {
                        self.bot.send_audio(chat_id, input_file).send().await
                    };

                    if let Err(error) = sent {
                        if should_retry_plain_caption(&error) {
                            tracing::debug!(
                                %error,
                                "entity caption send failed, retrying telegram audio with plain caption"
                            );
                            let fallback_file = InputFile::memory(data).file_name(filename);
                            let mut request = self.bot.send_audio(chat_id, fallback_file);
                            if let Some(caption_text) = caption {
                                request = request.caption(caption_text);
                            }
                            request
                                .send()
                                .await
                                .context("failed to send telegram audio")?;
                        } else {
                            return Err(error).context("failed to send telegram audio caption")?;
                        }
                    }
                } else {
                    let input_file = InputFile::memory(data.clone()).file_name(filename.clone());
                    let sent = if let Some(ref caption_text) = caption {
                        let rendered_caption = markdown_to_telegram_entities(caption_text);
                        self.bot
                            .send_document(chat_id, input_file)
                            .caption(rendered_caption.text)
                            .caption_entities(rendered_caption.entities)
                            .send()
                            .await
                    } else {
                        self.bot.send_document(chat_id, input_file).send().await
                    };

                    if let Err(error) = sent {
                        if should_retry_plain_caption(&error) {
                            tracing::debug!(
                                %error,
                                "entity caption send failed, retrying telegram file with plain caption"
                            );
                            let fallback_file = InputFile::memory(data).file_name(filename);
                            let mut request = self.bot.send_document(chat_id, fallback_file);
                            if let Some(caption_text) = caption {
                                request = request.caption(caption_text);
                            }
                            request
                                .send()
                                .await
                                .context("failed to send telegram file")?;
                        } else {
                            return Err(error).context("failed to send telegram file caption")?;
                        }
                    }
                }
            }
            OutboundResponse::Reaction(emoji) => {
                let message_id = self.extract_message_id(message)?;

                let reaction = ReactionType::Emoji {
                    emoji: emoji.clone(),
                };
                if let Err(error) = self
                    .bot
                    .set_message_reaction(chat_id, message_id)
                    .reaction(vec![reaction])
                    .send()
                    .await
                {
                    // Telegram only supports a limited set of reaction emojis per chat.
                    // Log and continue rather than failing the response.
                    tracing::debug!(
                        %error,
                        emoji = %emoji,
                        "failed to set telegram reaction (emoji may not be available in this chat)"
                    );
                }
            }
            OutboundResponse::StreamStart => {
                self.stop_typing(&message.conversation_id).await;

                let placeholder = self
                    .bot
                    .send_message(chat_id, "...")
                    .send()
                    .await
                    .context("failed to send stream placeholder")?;

                self.active_messages.write().await.insert(
                    message.conversation_id.clone(),
                    ActiveStream {
                        chat_id,
                        message_id: placeholder.id,
                        last_edit: Instant::now(),
                    },
                );
            }
            OutboundResponse::StreamChunk(text) => {
                let mut active = self.active_messages.write().await;
                if let Some(stream) = active.get_mut(&message.conversation_id) {
                    if stream.last_edit.elapsed() < STREAM_EDIT_INTERVAL {
                        return Ok(());
                    }

                    let display_text = if text.len() > MAX_MESSAGE_LENGTH {
                        let end = text.floor_char_boundary(MAX_MESSAGE_LENGTH - 3);
                        format!("{}...", &text[..end])
                    } else {
                        text
                    };

                    let rendered = markdown_to_telegram_entities(&display_text);
                    if let Err(html_error) = self
                        .bot
                        .edit_message_text(stream.chat_id, stream.message_id, rendered.text)
                        .entities(rendered.entities)
                        .send()
                        .await
                    {
                        tracing::debug!(%html_error, "entity edit failed, retrying as plain text");
                        if let Err(error) = self
                            .bot
                            .edit_message_text(stream.chat_id, stream.message_id, &display_text)
                            .send()
                            .await
                        {
                            tracing::debug!(%error, "failed to edit streaming message");
                        }
                    }
                    stream.last_edit = Instant::now();
                }
            }
            OutboundResponse::StreamEnd => {
                self.active_messages
                    .write()
                    .await
                    .remove(&message.conversation_id);
            }
            OutboundResponse::Status(status) => {
                self.send_status(message, status).await?;
            }
            // Slack-specific variants — graceful fallbacks for Telegram
            OutboundResponse::RemoveReaction(_) => {} // no-op
            OutboundResponse::Ephemeral { text, .. } => {
                // Telegram has no ephemeral messages — send as regular text
                send_formatted(&self.bot, chat_id, &text, None).await?;
            }
            OutboundResponse::ScheduledMessage { text, .. } => {
                // Telegram has no scheduled messages — send immediately
                send_formatted(&self.bot, chat_id, &text, None).await?;
            }
        }

        Ok(())
    }

    async fn send_status(
        &self,
        message: &InboundMessage,
        status: StatusUpdate,
    ) -> crate::Result<()> {
        match status {
            StatusUpdate::Thinking => {
                let chat_id = self.extract_chat_id(message)?;
                let bot = self.bot.clone();
                let conversation_id = message.conversation_id.clone();

                // Telegram typing indicators expire after 5 seconds.
                // Send one immediately, then repeat every 4 seconds.
                let handle = tokio::spawn(async move {
                    loop {
                        if let Err(error) = bot
                            .send_chat_action(chat_id, ChatAction::Typing)
                            .send()
                            .await
                        {
                            tracing::debug!(%error, "failed to send typing indicator");
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                    }
                });

                self.typing_tasks
                    .write()
                    .await
                    .insert(conversation_id, handle);
            }
            _ => {
                self.stop_typing(&message.conversation_id).await;
            }
        }

        Ok(())
    }

    async fn broadcast(&self, target: &str, response: OutboundResponse) -> crate::Result<()> {
        let chat_id = ChatId(
            target
                .parse::<i64>()
                .context("invalid telegram chat id for broadcast target")?,
        );

        if let OutboundResponse::Text(text) = response {
            send_formatted(&self.bot, chat_id, &text, None).await?;
        } else if let OutboundResponse::RichMessage { text, poll, .. } = response {
            send_formatted(&self.bot, chat_id, &text, None).await?;

            if let Some(poll_data) = poll {
                send_poll(&self.bot, chat_id, &poll_data).await?;
            }
        }

        Ok(())
    }

    async fn health_check(&self) -> crate::Result<()> {
        self.bot
            .get_me()
            .send()
            .await
            .context("telegram health check failed")?;
        Ok(())
    }

    async fn shutdown(&self) -> crate::Result<()> {
        // Cancel all typing indicator tasks
        let mut tasks = self.typing_tasks.write().await;
        for (_, handle) in tasks.drain() {
            handle.abort();
        }

        // Signal the polling loop to stop
        if let Some(tx) = self.shutdown_tx.read().await.as_ref() {
            tx.send(()).await.ok();
        }

        tracing::info!("telegram adapter shut down");
        Ok(())
    }
}

// -- Helper functions --

/// Extract text content from a Telegram message.
fn extract_text(message: &teloxide::types::Message) -> Option<String> {
    match &message.kind {
        MessageKind::Common(common) => match &common.media_kind {
            MediaKind::Text(text) => Some(text.text.clone()),
            MediaKind::Photo(photo) => photo.caption.clone(),
            MediaKind::Document(doc) => doc.caption.clone(),
            MediaKind::Video(video) => video.caption.clone(),
            MediaKind::Voice(voice) => voice.caption.clone(),
            MediaKind::Audio(audio) => audio.caption.clone(),
            _ => None,
        },
        _ => None,
    }
}

/// Check if a message contains file attachments.
fn has_attachments(message: &teloxide::types::Message) -> bool {
    match &message.kind {
        MessageKind::Common(common) => matches!(
            &common.media_kind,
            MediaKind::Photo(_)
                | MediaKind::Document(_)
                | MediaKind::Video(_)
                | MediaKind::Voice(_)
                | MediaKind::Audio(_)
        ),
        _ => false,
    }
}

/// Build `MessageContent` from a Telegram message.
///
/// Resolves Telegram file IDs to download URLs via the Bot API.
async fn build_content(
    bot: &Bot,
    message: &teloxide::types::Message,
    text: &Option<String>,
) -> MessageContent {
    let attachments = extract_attachments(message);

    if attachments.is_empty() {
        return MessageContent::Text(text.clone().unwrap_or_default());
    }

    let mut resolved = Vec::with_capacity(attachments.len());
    for mut attachment in attachments {
        match resolve_file_url(bot, &attachment.url).await {
            Ok(url) => attachment.url = url,
            Err(error) => {
                tracing::warn!(
                    file_id = %attachment.url,
                    %error,
                    "failed to resolve telegram file URL, skipping attachment"
                );
                continue;
            }
        }
        resolved.push(attachment);
    }

    if resolved.is_empty() {
        MessageContent::Text(text.clone().unwrap_or_default())
    } else {
        MessageContent::Media {
            text: text.clone(),
            attachments: resolved,
        }
    }
}

/// Extract file attachment metadata from a Telegram message.
fn extract_attachments(message: &teloxide::types::Message) -> Vec<Attachment> {
    let mut attachments = Vec::new();

    let MessageKind::Common(common) = &message.kind else {
        return attachments;
    };

    match &common.media_kind {
        MediaKind::Photo(photo) => {
            // Use the largest photo size
            if let Some(largest) = photo.photo.last() {
                attachments.push(Attachment {
                    filename: format!("photo_{}.jpg", largest.file.unique_id),
                    mime_type: "image/jpeg".into(),
                    url: largest.file.id.to_string(),
                    size_bytes: Some(largest.file.size as u64),
                    auth_header: None,
                });
            }
        }
        MediaKind::Document(doc) => {
            attachments.push(Attachment {
                filename: doc
                    .document
                    .file_name
                    .clone()
                    .unwrap_or_else(|| "document".into()),
                mime_type: doc
                    .document
                    .mime_type
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "application/octet-stream".into()),
                url: doc.document.file.id.to_string(),
                size_bytes: Some(doc.document.file.size as u64),
                auth_header: None,
            });
        }
        MediaKind::Video(video) => {
            attachments.push(Attachment {
                filename: video
                    .video
                    .file_name
                    .clone()
                    .unwrap_or_else(|| "video.mp4".into()),
                mime_type: video
                    .video
                    .mime_type
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "video/mp4".into()),
                url: video.video.file.id.to_string(),
                size_bytes: Some(video.video.file.size as u64),
                auth_header: None,
            });
        }
        MediaKind::Voice(voice) => {
            attachments.push(Attachment {
                filename: "voice.ogg".into(),
                mime_type: voice
                    .voice
                    .mime_type
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "audio/ogg".into()),
                url: voice.voice.file.id.to_string(),
                size_bytes: Some(voice.voice.file.size as u64),
                auth_header: None,
            });
        }
        MediaKind::Audio(audio) => {
            attachments.push(Attachment {
                filename: audio
                    .audio
                    .file_name
                    .clone()
                    .unwrap_or_else(|| "audio".into()),
                mime_type: audio
                    .audio
                    .mime_type
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "audio/mpeg".into()),
                url: audio.audio.file.id.to_string(),
                size_bytes: Some(audio.audio.file.size as u64),
                auth_header: None,
            });
        }
        _ => {}
    }

    attachments
}

/// Resolve a Telegram file ID to a download URL via the Bot API.
///
/// Telegram doesn't provide direct URLs for file attachments. Instead you get a file ID
/// that must be resolved through `getFile` to obtain the actual download path.
async fn resolve_file_url(bot: &Bot, file_id: &str) -> anyhow::Result<String> {
    let file = bot
        .get_file(FileId(file_id.to_string()))
        .send()
        .await
        .context("getFile API call failed")?;

    let mut url = bot.api_url();
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("cannot-be-a-base URL"))?;
        segments.push("file");
        segments.push(&format!("bot{}", bot.token()));
        segments.push(&file.path);
    }

    Ok(url.to_string())
}

/// Build platform-specific metadata for a Telegram message.
fn build_metadata(
    message: &teloxide::types::Message,
    bot_username: &Option<String>,
) -> (HashMap<String, serde_json::Value>, Option<String>) {
    let mut metadata = HashMap::new();

    metadata.insert(
        "telegram_chat_id".into(),
        serde_json::Value::Number(message.chat.id.0.into()),
    );
    metadata.insert(
        "telegram_message_id".into(),
        serde_json::Value::Number(message.id.0.into()),
    );
    metadata.insert(
        crate::metadata_keys::MESSAGE_ID.into(),
        serde_json::Value::String(message.id.0.to_string()),
    );

    let chat_type = if message.chat.is_private() {
        "private"
    } else if message.chat.is_group() {
        "group"
    } else if message.chat.is_supergroup() {
        "supergroup"
    } else if message.chat.is_channel() {
        "channel"
    } else {
        "unknown"
    };
    metadata.insert("telegram_chat_type".into(), chat_type.into());

    if let Some(title) = &message.chat.title() {
        metadata.insert("telegram_chat_title".into(), (*title).into());
        metadata.insert(crate::metadata_keys::SERVER_NAME.into(), (*title).into());
    }
    let channel_name = message
        .chat
        .title()
        .map(|title| title.to_string())
        .or_else(|| message.from.as_ref().map(build_display_name))
        .unwrap_or_else(|| chat_type.to_string());
    metadata.insert(
        crate::metadata_keys::CHANNEL_NAME.into(),
        channel_name.into(),
    );

    let formatted_author = if let Some(from) = &message.from {
        metadata.insert(
            "telegram_user_id".into(),
            serde_json::Value::Number(from.id.0.into()),
        );

        let display_name = build_display_name(from);
        metadata.insert("display_name".into(), display_name.clone().into());
        metadata.insert("sender_display_name".into(), display_name.clone().into());

        let author = if let Some(username) = &from.username {
            metadata.insert("telegram_username".into(), username.clone().into());
            metadata.insert(
                "telegram_user_mention".into(),
                serde_json::Value::String(format!("@{}", username)),
            );
            format!("{} (@{})", display_name, username)
        } else {
            display_name
        };
        Some(author)
    } else {
        None
    };

    if let Some(bot_username) = bot_username {
        metadata.insert("telegram_bot_username".into(), bot_username.clone().into());
    }

    // Compute combined mentions-or-replies-to-bot flag for require_mention.
    // Matches the pattern used by Discord/Slack/Twitch adapters.
    let mut mentions_or_replies_to_bot = false;

    // Check text-based @mention in message text/caption.
    // Uses a word-boundary check so "@spacebot" doesn't match "@spacebot_extra".
    if let Some(bot_username) = bot_username {
        let bot_lower = bot_username.to_lowercase();
        if let Some(text) = extract_text(message) {
            let text_lower = text.to_lowercase();
            let mention = format!("@{bot_lower}");
            // Telegram usernames can contain [a-z0-9_], so ensure the character
            // after the mention (if any) is not a valid username character.
            if let Some(start) = text_lower.find(&mention) {
                let after = start + mention.len();
                let is_boundary = text_lower
                    .as_bytes()
                    .get(after)
                    .is_none_or(|&ch| !ch.is_ascii_alphanumeric() && ch != b'_');
                if is_boundary {
                    mentions_or_replies_to_bot = true;
                }
            }
        }
    }

    // Reply-to context for threading
    let mut reply_to_is_bot_match = false;
    if let Some(reply) = message.reply_to_message() {
        metadata.insert(
            "reply_to_message_id".into(),
            serde_json::Value::Number(reply.id.0.into()),
        );
        if let Some(text) = extract_text(reply) {
            let truncated = if text.len() > 200 {
                format!("{}...", &text[..text.floor_char_boundary(197)])
            } else {
                text
            };
            metadata.insert("reply_to_text".into(), truncated.into());
        }
        if let Some(from) = &reply.from {
            metadata.insert("reply_to_author".into(), build_display_name(from).into());
            metadata.insert(
                "reply_to_user_id".into(),
                serde_json::Value::Number(from.id.0.into()),
            );
            metadata.insert(
                "reply_to_is_bot".into(),
                serde_json::Value::Bool(from.is_bot),
            );
            if let Some(username) = &from.username {
                metadata.insert("reply_to_username".into(), username.clone().into());
                // Check if reply is to our bot specifically
                if from.is_bot
                    && let Some(bot_username) = bot_username
                    && username.to_lowercase() == bot_username.to_lowercase()
                {
                    reply_to_is_bot_match = true;
                }
            }
        }
    }

    if !mentions_or_replies_to_bot && reply_to_is_bot_match {
        mentions_or_replies_to_bot = true;
    }
    metadata.insert(
        "telegram_mentions_or_replies_to_bot".into(),
        serde_json::Value::Bool(mentions_or_replies_to_bot),
    );

    (metadata, formatted_author)
}

/// Build a display name from a Telegram user, preferring full name.
fn build_display_name(user: &teloxide::types::User) -> String {
    let first = &user.first_name;
    match &user.last_name {
        Some(last) => format!("{first} {last}"),
        None => first.clone(),
    }
}

/// Send a native Telegram poll.
///
/// Telegram limits: max 12 answer options, question max 300 chars, each option
/// max 100 chars. `open_period` only supports 5–600 seconds so we only set it
/// when `duration_hours` converts to ≤600s; otherwise the poll stays open
/// indefinitely (until manually stopped via the Telegram client).
async fn send_poll(bot: &Bot, chat_id: ChatId, poll: &crate::Poll) -> anyhow::Result<()> {
    let question = if poll.question.len() > 300 {
        format!(
            "{}…",
            &poll.question[..poll.question.floor_char_boundary(299)]
        )
    } else {
        poll.question.clone()
    };

    let options: Vec<InputPollOption> = poll
        .answers
        .iter()
        .take(12)
        .map(|answer| {
            let text = if answer.len() > 100 {
                format!("{}…", &answer[..answer.floor_char_boundary(99)])
            } else {
                answer.clone()
            };
            InputPollOption::new(text)
        })
        .collect();

    if options.len() < 2 {
        anyhow::bail!("telegram polls require at least 2 answer options");
    }

    let mut request = bot
        .send_poll(chat_id, question, options)
        .is_anonymous(false);

    // Telegram's open_period only supports 5–600 seconds. Apply it when the
    // requested duration fits; otherwise leave unset so the poll stays open
    // indefinitely.
    let duration_secs = poll.duration_hours.saturating_mul(3600);
    if (5..=600).contains(&duration_secs) {
        request = request.open_period(duration_secs as u16);
    }

    if poll.allow_multiselect {
        request = request.allows_multiple_answers(true);
    }

    request
        .send()
        .await
        .context("failed to send telegram poll")?;

    Ok(())
}

/// Return true when Telegram rejected rich text entities and a plain-caption retry is safe.
fn should_retry_plain_caption(error: &RequestError) -> bool {
    matches!(error, RequestError::Api(ApiError::CantParseEntities(_)))
}

// -- Markdown-to-Telegram formatting --

/// Normalize common prose and list-spacing issues before markdown parsing so
/// Telegram still renders readable structure when the model emits inline lists.
fn normalize_telegram_markdown(markdown: &str) -> String {
    let mut normalized = String::with_capacity(markdown.len() + markdown.len() / 8);
    let mut in_fenced_code_block = false;

    for segment in markdown.split_inclusive('\n') {
        let (line, newline) = match segment.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (segment, ""),
        };
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            normalized.push_str(line);
            normalized.push_str(newline);
            in_fenced_code_block = !in_fenced_code_block;
            continue;
        }

        if in_fenced_code_block {
            normalized.push_str(line);
            normalized.push_str(newline);
            continue;
        }

        let line = normalize_markdown_line_outside_inline_code(line);

        normalized.push_str(&line);
        normalized.push_str(newline);
    }

    normalized
}

/// Apply prose and boundary repairs only to the non-code portions of a single
/// markdown line so inline code spans remain byte-for-byte intact.
fn normalize_markdown_line_outside_inline_code(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len() + line.len() / 8);
    let mut index = 0;

    while index < line.len() {
        let Some(backtick_offset) = line[index..].find('`') else {
            let trailing_segment = &line[index..];
            if normalized.ends_with('`') {
                normalized.push_str(&normalize_plain_segment_after_inline_code(trailing_segment));
            } else {
                normalized.push_str(&normalize_plain_markdown_line(trailing_segment));
            }
            break;
        };

        let backtick_start = index + backtick_offset;
        let plain_segment = &line[index..backtick_start];
        if normalized.ends_with('`') {
            normalized.push_str(&normalize_plain_segment_after_inline_code(plain_segment));
        } else {
            normalized.push_str(&normalize_plain_markdown_line(plain_segment));
        }

        let delimiter_len = count_leading_backticks(&line[backtick_start..]);
        let delimiter = &line[backtick_start..backtick_start + delimiter_len];
        normalized.push_str(delimiter);

        let code_start = backtick_start + delimiter_len;
        let Some(code_end) = find_matching_backtick_delimiter(line, code_start, delimiter_len)
        else {
            normalized.push_str(&line[code_start..]);
            break;
        };

        normalized.push_str(&line[code_start..code_end]);
        normalized.push_str(delimiter);
        index = code_end + delimiter_len;
    }

    normalized
}

fn count_leading_backticks(text: &str) -> usize {
    text.chars()
        .take_while(|character| *character == '`')
        .count()
}

fn find_matching_backtick_delimiter(
    line: &str,
    search_start: usize,
    delimiter_len: usize,
) -> Option<usize> {
    let mut search_index = search_start;

    while search_index < line.len() {
        let offset = line[search_index..].find('`')?;
        let candidate_start = search_index + offset;
        let candidate_len = count_leading_backticks(&line[candidate_start..]);
        if candidate_len == delimiter_len {
            return Some(candidate_start);
        }
        search_index = candidate_start + candidate_len;
    }

    None
}

fn normalize_plain_markdown_line(line: &str) -> String {
    let mut line = normalize_prose_spacing(line);
    line = normalize_token_boundaries(&line);
    line = normalize_inline_list_boundaries(&line);
    line = normalize_emphasized_block_boundaries(&line);
    line = normalize_list_item_tail_boundaries(&line);
    line = normalize_ordered_list_body_boundaries(&line);
    escape_literal_angle_bracket_emails(&line)
}

/// Repair boundaries that occur immediately after an inline code span before
/// the following plain-text segment is normalized normally.
fn normalize_plain_segment_after_inline_code(segment: &str) -> String {
    let leading_whitespace_len = segment
        .char_indices()
        .find_map(|(index, character)| (!matches!(character, ' ' | '\t')).then_some(index))
        .unwrap_or(segment.len());
    let (leading_whitespace, content) = segment.split_at(leading_whitespace_len);

    let mut normalized = String::with_capacity(segment.len() + segment.len() / 8);
    normalized.push_str(leading_whitespace);
    if leading_whitespace.is_empty() {
        if starts_block_section_label(content) {
            normalized.push_str("\n\n");
        } else if starts_compact_list_marker(content) {
            normalized.push('\n');
        } else if leading_sentence_starter_word_len(content).is_some() {
            normalized.push(' ');
        }
    }
    normalized.push_str(content);
    normalize_plain_markdown_line(&normalized)
}

/// Repair compact prose spacing such as `March12`, `at8:00`, `The3`, and
/// `questions13-15` without touching all-caps model/version tokens.
fn normalize_prose_spacing(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len() + line.len() / 8);

    for (byte_index, character) in line.char_indices() {
        normalized.push(character);

        let next_index = byte_index + character.len_utf8();
        if next_index >= line.len() {
            continue;
        }

        let next_character = line[next_index..]
            .chars()
            .next()
            .expect("next character exists");
        if next_character.is_whitespace() {
            continue;
        }

        if should_insert_space_after_sentence_punctuation(character, next_character)
            || should_insert_space_after_comma(line, byte_index, next_character)
            || should_insert_space_between_word_and_number(line, byte_index, next_character)
        {
            normalized.push(' ');
        }
    }

    normalized
}

/// Repair missing boundaries between a completed token and the next sentence or
/// section label, while keeping the fixes narrow to obvious prose transitions.
fn normalize_token_boundaries(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len() + line.len() / 8);
    let mut index = 0;

    while index < line.len() {
        if let Some((closing_start, closing_end)) = find_emphasis_span(line, index) {
            let span = &line[index..closing_end];
            let inner = &line[index + 2..closing_start];
            let previous = previous_non_whitespace_char(&normalized);
            let following = &line[closing_end..];

            if starts_compact_list_marker(following)
                && is_block_heading_candidate(inner, &normalized, previous, following)
            {
                insert_heading_break_if_needed(&mut normalized, previous);
                normalized.push_str(span);
                if starts_compact_list_marker(following) && !normalized.ends_with('\n') {
                    normalized.push('\n');
                }
            } else {
                normalized.push_str(span);
            }
            index = closing_end;
            continue;
        }

        let slice = &line[index..];
        if should_insert_section_break(&normalized, slice) {
            trim_trailing_horizontal_whitespace(&mut normalized);
            insert_section_break_if_needed(&mut normalized);
        } else if should_insert_space_before_sentence_starter(&normalized, slice) {
            trim_trailing_horizontal_whitespace(&mut normalized);
            if !normalized.ends_with([' ', '\n']) {
                normalized.push(' ');
            }
        }

        let character = slice.chars().next().expect("valid utf-8 boundary");
        normalized.push(character);
        index += character.len_utf8();
    }

    normalized
}

/// Repair emphasized block boundaries such as `summary:**Heading**1.` or
/// `summary:**Field:** value**Next:** value` before pulldown-cmark parses the markdown.
fn normalize_emphasized_block_boundaries(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len() + line.len() / 8);
    let mut index = 0;

    while index < line.len() {
        if let Some((closing_start, closing_end)) = find_emphasis_span(line, index) {
            let span = &line[index..closing_end];
            let inner = &line[index + 2..closing_start];
            let previous = previous_non_whitespace_char(&normalized);
            let following = &line[closing_end..];
            let following_trimmed = following.trim_start_matches([' ', '\t']);

            if is_field_label(inner) {
                insert_field_break_if_needed(&mut normalized, previous);
                normalized.push_str(span);
                if starts_emphasized_block_break(following_trimmed) && !normalized.ends_with('\n') {
                    normalized.push('\n');
                } else if should_insert_space_after_emphasized_label(following) {
                    normalized.push(' ');
                }
            } else if is_block_heading_candidate(inner, &normalized, previous, following_trimmed) {
                insert_heading_break_if_needed(&mut normalized, previous);
                normalized.push_str(span);
                if starts_emphasized_block_break(following_trimmed) && !normalized.ends_with('\n') {
                    normalized.push('\n');
                }
            } else {
                normalized.push_str(span);
                if should_insert_space_after_emphasis_span(inner, following) {
                    normalized.push(' ');
                }
            }

            index = closing_end;
            continue;
        }

        let character = line[index..].chars().next().expect("valid utf-8 boundary");
        normalized.push(character);
        index += character.len_utf8();
    }

    normalized
}

fn is_field_label(inner: &str) -> bool {
    let trimmed = inner.trim();
    let Some(label) = trimmed.strip_suffix(':').map(str::trim) else {
        return false;
    };
    if label.is_empty() {
        return false;
    }

    let word_count = label.split_whitespace().count();
    if !(1..=4).contains(&word_count) {
        return false;
    }

    label
        .chars()
        .find(|character| character.is_ascii_alphanumeric())
        .is_some_and(|character| character.is_ascii_uppercase())
}

fn find_emphasis_span(line: &str, index: usize) -> Option<(usize, usize)> {
    let delimiter = if line[index..].starts_with("**") {
        "**"
    } else if line[index..].starts_with("__") {
        "__"
    } else {
        return None;
    };

    // Leave triple emphasis alone; it is valid inline markdown but not a block heading.
    if line[index..].starts_with("***") || line[index..].starts_with("___") {
        return None;
    }

    let content_start = index + delimiter.len();
    let close_offset = line[content_start..].find(delimiter)?;
    let closing_start = content_start + close_offset;
    let closing_end = closing_start + delimiter.len();
    Some((closing_start, closing_end))
}

fn is_block_heading_candidate(
    inner: &str,
    prefix: &str,
    previous: Option<char>,
    following: &str,
) -> bool {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return false;
    }

    let word_count = trimmed.split_whitespace().count();
    let starts_heading_case = trimmed
        .chars()
        .find(|character| character.is_alphanumeric())
        .is_some_and(|character| character.is_uppercase());
    let looks_like_heading = trimmed.ends_with(':')
        || trimmed.contains(" / ")
        || (starts_heading_case && word_count >= 2);

    if !looks_like_heading {
        return false;
    }

    if ends_with_ordered_list_marker(prefix) {
        return false;
    }

    starts_compact_list_marker(following) || matches!(previous, Some(':' | ';' | '.' | '!' | '?'))
}

fn previous_non_whitespace_char(text: &str) -> Option<char> {
    text.chars()
        .rev()
        .find(|character| !character.is_whitespace())
}

fn ends_with_ordered_list_marker(text: &str) -> bool {
    let trimmed = text.trim_end();
    let tail = trimmed
        .rsplit('\n')
        .next()
        .unwrap_or(trimmed)
        .trim_start_matches([' ', '\t']);

    ordered_list_marker_len(tail).is_some_and(|marker_len| marker_len == tail.len())
}

fn insert_heading_break_if_needed(text: &mut String, previous: Option<char>) {
    if text.is_empty() || text.ends_with("\n\n") {
        return;
    }

    if text.ends_with('\n') {
        text.push('\n');
        return;
    }

    if previous.is_some() {
        if matches!(previous, Some(':' | ';' | '.' | '!' | '?')) {
            text.push_str("\n\n");
        } else {
            text.push('\n');
        }
    } else if previous_non_whitespace_char(text).is_some() {
        text.push_str("\n\n");
    }
}

fn insert_field_break_if_needed(text: &mut String, previous: Option<char>) {
    if text.is_empty() || text.ends_with('\n') {
        return;
    }

    if matches!(previous, Some(':' | ';' | '.' | '!' | '?')) {
        text.push_str("\n\n");
    } else {
        text.push('\n');
    }
}

fn starts_compact_list_marker(text: &str) -> bool {
    let trimmed = text.trim_start_matches([' ', '\t']);

    if let Some(rest) = trimmed.strip_prefix('*') {
        return if rest.starts_with([' ', '\t']) {
            rest.trim_start_matches([' ', '\t'])
                .chars()
                .next()
                .is_some_and(|character| !character.is_ascii_lowercase())
        } else {
            rest.chars().next().is_some_and(|character| {
                character == '`' || character == '#' || character == '*' || character == '_'
            })
        };
    }

    if trimmed.starts_with('-') || trimmed.starts_with('•') {
        return trimmed[1..].chars().next().is_some_and(|character| {
            character.is_whitespace()
                || character == '`'
                || character == '#'
                || character.is_ascii_alphabetic()
        });
    }

    ordered_list_marker_len(trimmed).is_some_and(|marker_len| {
        trimmed[marker_len..]
            .chars()
            .next()
            .is_some_and(|character| {
                character.is_whitespace()
                    || character == '*'
                    || character == '_'
                    || character == '`'
                    || character == '#'
                    || character.is_ascii_alphabetic()
            })
    })
}

fn starts_emphasized_block_break(text: &str) -> bool {
    starts_compact_list_marker(text)
        || text.starts_with('>')
        || text.starts_with("```")
        || starts_emphasized_field_label(text)
}

fn starts_unordered_or_ordered_list_marker(text: &str) -> bool {
    let trimmed = text.trim_start_matches([' ', '\t']);

    if trimmed.starts_with('-') || trimmed.starts_with('•') {
        return trimmed[1..].chars().next().is_some_and(|character| {
            character.is_whitespace()
                || character == '`'
                || character == '#'
                || character.is_ascii_alphabetic()
        });
    }

    if let Some(rest) = trimmed.strip_prefix('*') {
        return rest.chars().next().is_some_and(char::is_whitespace);
    }

    ordered_list_marker_len(trimmed).is_some_and(|marker_len| {
        trimmed[marker_len..]
            .chars()
            .next()
            .is_some_and(|character| {
                character.is_whitespace()
                    || character == '*'
                    || character == '_'
                    || character == '`'
                    || character == '#'
                    || character.is_ascii_alphabetic()
            })
    })
}

fn starts_block_followup(text: &str) -> bool {
    let trimmed = text.trim_start_matches([' ', '\t']);
    starts_unordered_or_ordered_list_marker(trimmed)
        || trimmed.starts_with('>')
        || trimmed.starts_with("```")
        || starts_emphasized_field_label(trimmed)
}

fn starts_emphasized_field_label(text: &str) -> bool {
    let Some((closing_start, _closing_end)) = find_emphasis_span(text, 0) else {
        return false;
    };
    is_field_label(&text[2..closing_start])
}

fn should_insert_space_after_emphasized_label(following: &str) -> bool {
    let Some(next_character) = following.chars().next() else {
        return false;
    };
    if next_character.is_whitespace() {
        return false;
    }

    let trimmed = following.trim_start_matches([' ', '\t']);
    !starts_emphasized_block_break(trimmed)
}

fn should_insert_space_after_emphasis_span(inner: &str, following: &str) -> bool {
    let Some(next_character) = following.chars().next() else {
        return false;
    };
    if next_character.is_whitespace() {
        return false;
    }

    let trimmed = following.trim_start_matches([' ', '\t']);
    leading_sentence_starter_word_len(trimmed).is_some()
        && emphasis_span_is_self_contained(inner)
        && !starts_emphasized_block_break(trimmed)
}

fn emphasis_span_is_self_contained(inner: &str) -> bool {
    let trimmed = inner.trim();
    !trimmed.is_empty()
        && (trimmed.split_whitespace().count() > 1
            || trimmed.contains(['.', ':', '-', '/', '_', '#'])
            || trimmed.chars().any(|character| character.is_ascii_digit()))
}

fn escape_literal_angle_bracket_emails(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    let mut index = 0;

    while let Some(start_offset) = line[index..].find('<') {
        let start = index + start_offset;
        let Some(end_offset) = line[start + 1..].find('>') else {
            break;
        };
        let end = start + 1 + end_offset;
        let candidate = &line[start + 1..end];

        if looks_like_literal_email(candidate) {
            normalized.push_str(&line[index..start]);
            normalized.push_str("&lt;");
            normalized.push_str(candidate);
            normalized.push_str("&gt;");
            index = end + 1;
            continue;
        }

        normalized.push_str(&line[index..=start]);
        index = start + 1;
    }

    normalized.push_str(&line[index..]);
    normalized
}

fn looks_like_literal_email(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.contains('@')
        && !candidate.chars().any(char::is_whitespace)
        && !candidate.contains(['<', '>'])
}

fn ordered_list_marker_len(text: &str) -> Option<usize> {
    let mut digit_length = 0;
    for character in text.chars() {
        if character.is_ascii_digit() {
            digit_length += character.len_utf8();
        } else {
            break;
        }
    }

    if digit_length == 0 || !text[digit_length..].starts_with('.') {
        return None;
    }

    Some(digit_length + 1)
}

/// Repair inline lists that were emitted mid-sentence instead of on their own
/// lines, but keep normal punctuation and prose intact.
fn normalize_inline_list_boundaries(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len() + line.len() / 8);
    let mut index = 0;

    while index < line.len() {
        if let Some((_, closing_end)) = find_emphasis_span(line, index) {
            normalized.push_str(&line[index..closing_end]);
            index = closing_end;
            continue;
        }

        let slice = &line[index..];
        if should_insert_list_break(&normalized, slice) {
            trim_trailing_horizontal_whitespace(&mut normalized);
            insert_list_break_if_needed(&mut normalized);
        }

        let character = slice.chars().next().expect("valid utf-8 boundary");
        normalized.push(character);
        index += character.len_utf8();
    }

    normalized
}

fn should_insert_space_after_sentence_punctuation(character: char, next_character: char) -> bool {
    matches!(character, '.' | '!' | '?') && next_character.is_ascii_uppercase()
}

fn should_insert_space_after_comma(line: &str, comma_index: usize, next_character: char) -> bool {
    if !line[comma_index..].starts_with(',') {
        return false;
    }
    if next_character.is_whitespace() {
        return false;
    }

    let previous_character = line[..comma_index].chars().next_back();
    match (previous_character, next_character) {
        (Some(previous), next)
            if previous.is_ascii_alphabetic() && next.is_ascii_alphanumeric() =>
        {
            true
        }
        (Some(previous), next) if previous.is_ascii_digit() && next.is_ascii_digit() => {
            let left_digits = count_ascii_digits_backward(line, comma_index);
            let right_digits = count_ascii_digits_forward(line, comma_index + 1);
            left_digits <= 2 || right_digits != 3
        }
        (Some(previous), next) if previous.is_ascii_digit() && next.is_ascii_alphabetic() => true,
        _ => false,
    }
}

fn should_insert_space_between_word_and_number(
    line: &str,
    boundary_index: usize,
    next_character: char,
) -> bool {
    if !next_character.is_ascii_digit() {
        return false;
    }

    let character = line[boundary_index..]
        .chars()
        .next()
        .expect("boundary character exists");
    if !character.is_ascii_alphabetic() {
        return false;
    }

    let token_start = token_start_before(line, boundary_index);
    let token_end = token_end_after(line, boundary_index + character.len_utf8());
    let token = &line[token_start..token_end];
    if token.contains("://") || token.contains('/') || token.contains('@') || token.contains('#') {
        return false;
    }

    let word = ascii_word_ending_at(line, boundary_index);
    if word.len() < 2 || word.chars().all(|character| character.is_ascii_uppercase()) {
        return false;
    }

    let next_slice = &line[boundary_index + character.len_utf8()..];
    if looks_like_version_number(next_slice) {
        return false;
    }

    true
}

fn count_ascii_digits_backward(text: &str, end: usize) -> usize {
    text[..end]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .count()
}

fn count_ascii_digits_forward(text: &str, start: usize) -> usize {
    text[start..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count()
}

fn token_start_before(text: &str, boundary_index: usize) -> usize {
    text[..boundary_index]
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            character
                .is_whitespace()
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0)
}

fn token_end_after(text: &str, boundary_index: usize) -> usize {
    text[boundary_index..]
        .char_indices()
        .find_map(|(index, character)| character.is_whitespace().then_some(boundary_index + index))
        .unwrap_or(text.len())
}

fn ascii_word_ending_at(text: &str, boundary_index: usize) -> &str {
    let start = text[..boundary_index]
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            (!matches!(character, 'A'..='Z' | 'a'..='z' | '\'' | '-' | '/'))
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    let end = boundary_index
        + text[boundary_index..]
            .chars()
            .next()
            .expect("boundary character exists")
            .len_utf8();
    &text[start..end]
}

fn looks_like_version_number(text: &str) -> bool {
    let mut characters = text.chars();
    let mut saw_digit = false;
    while let Some(character) = characters.next() {
        if character.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        if saw_digit && character == '.' {
            return characters.next().is_some_and(|next| next.is_ascii_digit());
        }
        return false;
    }
    false
}

fn should_insert_section_break(output: &str, slice: &str) -> bool {
    if output.is_empty() || output.ends_with('\n') {
        return false;
    }
    if !starts_block_section_label(slice) {
        return false;
    }

    previous_non_whitespace_char(output).is_some_and(is_token_ending_character)
}

fn should_insert_space_before_sentence_starter(output: &str, slice: &str) -> bool {
    if output.is_empty() || output.ends_with([' ', '\n']) || starts_section_label(slice).is_some() {
        return false;
    }
    if leading_sentence_starter_word_len(slice).is_none() {
        return false;
    }

    matches!(
        previous_non_whitespace_char(output),
        Some(character)
            if character.is_ascii_digit()
                || matches!(character, ')' | ']' | '>' | ':' | ';' | '.' | '!' | '?' | '`')
    )
}

fn should_insert_list_break(output: &str, slice: &str) -> bool {
    if output.is_empty() || output.ends_with('\n') || !starts_compact_list_marker(slice) {
        return false;
    }

    let Some(previous_character) = previous_non_whitespace_char(output) else {
        return false;
    };

    if ordered_list_marker_len(slice.trim_start_matches([' ', '\t'])).is_some() {
        return is_token_ending_character(previous_character)
            || matches!(previous_character, ':' | ';' | '.' | '!' | '?');
    }

    matches!(
        previous_character,
        ')' | ']' | '*' | '`' | ':' | ';' | '.' | '!' | '?'
    ) || (is_token_ending_character(previous_character) && unordered_marker_has_explicit_gap(slice))
}

fn unordered_marker_has_explicit_gap(text: &str) -> bool {
    let trimmed = text.trim_start_matches([' ', '\t']);
    let rest = if let Some(rest) = trimmed.strip_prefix('-') {
        rest
    } else if let Some(rest) = trimmed.strip_prefix('*') {
        rest
    } else if let Some(rest) = trimmed.strip_prefix('•') {
        rest
    } else {
        return false;
    };

    rest.chars().next().is_some_and(char::is_whitespace)
}

fn normalize_list_item_tail_boundaries(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len() + text.len() / 8);

    for segment in text.split_inclusive('\n') {
        let (line, newline) = match segment.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (segment, ""),
        };

        normalized.push_str(&normalize_single_list_item_line(line));
        normalized.push_str(newline);
    }

    normalized
}

fn normalize_ordered_list_body_boundaries(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut normalized_lines = Vec::with_capacity(lines.len());
    let mut index = 0;

    while index < lines.len() {
        let trimmed = lines[index].trim();
        let is_marker_only =
            ordered_list_marker_len(trimmed).is_some_and(|marker_len| marker_len == trimmed.len());
        if is_marker_only {
            let mut next_index = index + 1;
            while next_index < lines.len() && lines[next_index].trim().is_empty() {
                next_index += 1;
            }

            if next_index < lines.len() {
                let next_line = lines[next_index].trim_start();
                let next_line_is_emphasis = find_emphasis_span(next_line, 0).is_some();
                if !next_line.is_empty()
                    && (!starts_compact_list_marker(next_line) || next_line_is_emphasis)
                {
                    normalized_lines.push(format!("{trimmed} {next_line}"));
                    index = next_index + 1;
                    continue;
                }
            }
        }

        normalized_lines.push(lines[index].to_string());
        index += 1;
    }

    normalized_lines.join("\n")
}

fn normalize_single_list_item_line(line: &str) -> String {
    if !starts_list_item_line(line) {
        return line.to_string();
    }

    let mut normalized = String::with_capacity(line.len() + line.len() / 8);
    let mut index = 0;

    while index < line.len() {
        if let Some((_, closing_end)) = find_emphasis_span(line, index) {
            normalized.push_str(&line[index..closing_end]);
            index = closing_end;
            continue;
        }

        let slice = &line[index..];
        if should_split_list_tail(&normalized, slice) {
            trim_trailing_horizontal_whitespace(&mut normalized);
            normalized.push_str("\n\n");
        }

        let character = slice.chars().next().expect("valid utf-8 boundary");
        normalized.push(character);
        index += character.len_utf8();
    }

    normalized
}

fn starts_list_item_line(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    trimmed.starts_with('-')
        || trimmed.starts_with('*')
        || trimmed.starts_with('•')
        || ordered_list_marker_len(trimmed).is_some()
}

fn should_split_list_tail(output: &str, slice: &str) -> bool {
    if output.is_empty() || output.ends_with('\n') {
        return false;
    }

    let Some(previous_character) = previous_non_whitespace_char(output) else {
        return false;
    };
    if matches!(previous_character, '.' | '!' | '?' | ':' | ';') {
        return false;
    }

    if starts_emphasized_heading_with_external_colon(slice) {
        return true;
    }

    if starts_sentence_after_list_tail(output, slice) {
        return true;
    }

    if !output.contains('—') {
        return false;
    }

    let Some(word_len) = leading_titlecase_word_len(slice) else {
        return false;
    };
    let rest = &slice[word_len..];
    if !rest.starts_with(' ') {
        return false;
    }

    rest.trim_start()
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
}

fn starts_emphasized_heading_with_external_colon(text: &str) -> bool {
    let Some((closing_start, closing_end)) = find_emphasis_span(text, 0) else {
        return false;
    };

    let inner = text[2..closing_start].trim();
    if !looks_like_compact_heading_label(inner) {
        return false;
    }

    text[closing_end..]
        .trim_start_matches([' ', '\t'])
        .starts_with(':')
}

fn looks_like_compact_heading_label(inner: &str) -> bool {
    if inner.is_empty() || inner.contains(['.', '/', '_']) || inner.split_whitespace().count() > 4 {
        return false;
    }

    inner
        .chars()
        .find(|character| character.is_ascii_alphanumeric())
        .is_some_and(|character| character.is_ascii_uppercase())
}

fn starts_sentence_after_list_tail(output: &str, slice: &str) -> bool {
    if leading_sentence_starter_word_len(slice).is_none() {
        return false;
    }

    if let Some(word) = trailing_ascii_alpha_token(output) {
        if word.chars().all(|character| character.is_ascii_lowercase()) {
            return true;
        }

        if word.len() <= 3 && word.chars().all(|character| character.is_ascii_uppercase()) {
            return true;
        }
    }

    previous_non_whitespace_char(output).is_some_and(|character| character.is_ascii_digit())
}

fn trailing_ascii_alpha_token(text: &str) -> Option<&str> {
    let end = text.char_indices().rev().find_map(|(index, character)| {
        (!character.is_whitespace()).then_some(index + character.len_utf8())
    })?;

    let start = text[..end]
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            (!character.is_ascii_alphabetic()).then_some(index + character.len_utf8())
        })
        .unwrap_or(0);

    let token = &text[start..end];
    token
        .chars()
        .all(|character| character.is_ascii_alphabetic())
        .then_some(token)
}

fn insert_section_break_if_needed(text: &mut String) {
    if text.is_empty() || text.ends_with("\n\n") {
        return;
    }
    if text.ends_with('\n') {
        text.push('\n');
    } else {
        text.push_str("\n\n");
    }
}

fn insert_list_break_if_needed(text: &mut String) {
    if text.is_empty() || text.ends_with('\n') {
        return;
    }

    match previous_non_whitespace_char(text) {
        Some(':' | ';' | '.' | '!' | '?') => text.push_str("\n\n"),
        _ => text.push('\n'),
    }
}

fn trim_trailing_horizontal_whitespace(text: &mut String) {
    while text.ends_with([' ', '\t']) {
        text.pop();
    }
}

fn is_token_ending_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, ')' | ']')
}

fn leading_titlecase_word_len(text: &str) -> Option<usize> {
    let mut characters = text.char_indices();
    let (_, first) = characters.next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }

    let mut end = first.len_utf8();
    let mut saw_lowercase = false;
    for (index, character) in characters {
        if !is_section_word_character(character) {
            break;
        }
        if character.is_ascii_lowercase() {
            saw_lowercase = true;
        }
        end = index + character.len_utf8();
    }

    saw_lowercase.then_some(end)
}

fn leading_sentence_starter_word_len(text: &str) -> Option<usize> {
    let word_len = leading_titlecase_word_len(text)?;
    let rest = &text[word_len..];
    if rest.is_empty() {
        return Some(word_len);
    }

    rest.chars()
        .next()
        .filter(|character| {
            character.is_whitespace()
                || matches!(character, '.' | ',' | ':' | ';' | '!' | '?' | ')' | ']')
        })
        .map(|_| word_len)
}

fn starts_section_label(text: &str) -> Option<usize> {
    let mut index = leading_section_label_head_len(text)?;
    let mut words = 1;

    loop {
        let rest = &text[index..];
        if let Some(':') = rest.chars().next() {
            return Some(index + 1);
        }
        if words == 3 || !rest.starts_with(' ') {
            return None;
        }

        index += 1;
        index += leading_section_label_tail_len(&text[index..])?;
        words += 1;
    }
}

fn starts_block_section_label(text: &str) -> bool {
    let Some(label_end) = starts_section_label(text) else {
        return false;
    };

    let following = text[label_end..].trim_start_matches([' ', '\t']);
    starts_block_followup(following)
}

fn leading_section_label_head_len(text: &str) -> Option<usize> {
    leading_acronym_len(text).or_else(|| leading_titlecase_word_len(text))
}

fn leading_section_label_tail_len(text: &str) -> Option<usize> {
    let mut characters = text.char_indices();
    let (_, first) = characters.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }

    let mut end = first.len_utf8();
    for (index, character) in characters {
        if !is_section_word_character(character) {
            break;
        }
        end = index + character.len_utf8();
    }

    Some(end)
}

fn leading_acronym_len(text: &str) -> Option<usize> {
    let mut characters = text.char_indices();
    let (_, first) = characters.next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }

    let mut end = first.len_utf8();
    let mut count = 1;
    for (index, character) in characters {
        if !character.is_ascii_uppercase() {
            break;
        }
        count += 1;
        end = index + character.len_utf8();
    }

    (count >= 2).then_some(end)
}

fn is_section_word_character(character: char) -> bool {
    character.is_ascii_alphabetic() || matches!(character, '\'' | '-' | '/')
}

/// Strip HTML tags and unescape entities for formatter unit tests.
#[cfg(test)]
fn strip_html_tags(html: &str) -> String {
    static TAG_PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<[^>]+>").expect("hardcoded regex"));
    TAG_PATTERN
        .replace_all(html, "")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[derive(Debug, Clone)]
enum ListContext {
    Unordered { has_items: bool },
    Ordered { next_index: u64, has_items: bool },
}

impl ListContext {
    fn mark_item_started(&mut self) -> bool {
        match self {
            Self::Unordered { has_items } | Self::Ordered { has_items, .. } => {
                let had_items = *has_items;
                *has_items = true;
                had_items
            }
        }
    }
}

#[derive(Debug, Default)]
struct TableState {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: Option<String>,
    in_header: bool,
}

impl TableState {
    fn start_row(&mut self) {
        self.current_row.clear();
    }

    fn start_cell(&mut self) {
        self.current_cell = Some(String::new());
    }

    fn push_cell_text(&mut self, text: &str) {
        if let Some(cell) = self.current_cell.as_mut() {
            cell.push_str(text);
        }
    }

    fn finish_cell(&mut self) {
        let cell = self.current_cell.take().unwrap_or_default();
        self.current_row.push(normalize_table_cell(&cell));
    }

    fn finish_row(&mut self) {
        if self.current_row.is_empty() {
            return;
        }

        let row = std::mem::take(&mut self.current_row);
        if self.in_header {
            self.headers = row;
        } else if row.iter().any(|cell| !cell.is_empty()) {
            self.rows.push(row);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedTelegramText {
    text: String,
    entities: Vec<MessageEntity>,
}

impl RenderedTelegramText {
    fn char_count(&self) -> usize {
        self.text.chars().count()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TelegramRenderMode {
    Entities,
    PlainTextFallback,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TelegramEntityTrace {
    pub kind: String,
    pub offset: usize,
    pub length: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TelegramRenderedChunkTrace {
    pub text: String,
    pub entities: Vec<TelegramEntityTrace>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TelegramRenderTrace {
    pub input: String,
    pub normalized: String,
    pub mode: TelegramRenderMode,
    pub rendered: TelegramRenderedChunkTrace,
    pub chunks: Vec<TelegramRenderedChunkTrace>,
}

#[derive(Debug)]
struct OpenEntity {
    kind: MessageEntityKind,
    offset: usize,
}

#[derive(Debug)]
struct TelegramEntityRenderer {
    output: String,
    output_utf16_len: usize,
    entities: Vec<MessageEntity>,
    open_entities: Vec<OpenEntity>,
    list_stack: Vec<ListContext>,
    list_item_depth: usize,
    blockquote_depth: usize,
    table_state: Option<TableState>,
}

impl TelegramEntityRenderer {
    fn new(capacity: usize) -> Self {
        Self {
            output: String::with_capacity(capacity),
            output_utf16_len: 0,
            entities: Vec::new(),
            open_entities: Vec::new(),
            list_stack: Vec::new(),
            list_item_depth: 0,
            blockquote_depth: 0,
            table_state: None,
        }
    }

    fn render(markdown: &str) -> RenderedTelegramText {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);

        let parser = Parser::new_ext(markdown, options);
        let mut renderer = Self::new(markdown.len());
        for event in parser {
            renderer.push_event(event);
        }
        renderer.finish()
    }

    fn finish(mut self) -> RenderedTelegramText {
        self.close_all_entities();
        let preserve_terminal_newline = self.entities.iter().any(|entity| {
            matches!(entity.kind, MessageEntityKind::Pre { .. })
                && entity.offset + entity.length == self.output_utf16_len
                && self.output.ends_with('\n')
        });
        self.trim_trailing_newlines_to(usize::from(preserve_terminal_newline));
        self.entities.sort_by(|left, right| {
            left.offset
                .cmp(&right.offset)
                .then(right.length.cmp(&left.length))
        });
        RenderedTelegramText {
            text: self.output,
            entities: self.entities,
        }
    }

    fn push_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) | Event::Html(text) => {
                if self.push_table_text(text.as_ref()) {
                    return;
                }
                self.push_text(text.as_ref());
            }
            Event::Code(text) => {
                if self.push_table_text(text.as_ref()) {
                    return;
                }
                self.push_inline_code(text.as_ref());
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.push_table_text(" ") {
                    return;
                }
                self.push_text("\n");
            }
            Event::Rule => {
                if self.in_list_item() {
                    self.ensure_line_break();
                } else {
                    self.ensure_blank_line();
                }
                self.push_text("──────────");
                self.close_block();
            }
            Event::TaskListMarker(checked) => {
                if self.push_table_text(if checked { "[x] " } else { "[ ] " }) {
                    return;
                }
                self.push_text(if checked { "[x] " } else { "[ ] " });
            }
            Event::FootnoteReference(reference) => {
                if self.push_table_footnote(reference.as_ref()) {
                    return;
                }
                self.push_text("[");
                self.push_text(reference.as_ref());
                self.push_text("]");
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        if self.handle_table_start_tag(&tag) {
            return;
        }

        match tag {
            Tag::Paragraph if !self.in_list_item() && self.blockquote_depth == 0 => {
                self.ensure_blank_line();
            }
            Tag::Paragraph => {}
            Tag::Heading(..) => {
                self.ensure_blank_line();
                self.open_entity(MessageEntityKind::Bold);
            }
            Tag::BlockQuote => {
                if self.output.ends_with('\n') && self.output.trim_end_matches('\n').ends_with(':')
                {
                    self.trim_trailing_newlines_to(1);
                } else {
                    self.ensure_blank_line();
                }
                self.blockquote_depth += 1;
                self.open_entity(MessageEntityKind::Blockquote);
            }
            Tag::CodeBlock(kind) => {
                if self.in_list_item() {
                    self.ensure_line_break();
                } else {
                    self.ensure_blank_line();
                }
                self.open_entity(MessageEntityKind::Pre {
                    language: code_block_language(&kind).map(str::to_owned),
                });
            }
            Tag::List(start) => {
                if self.in_list_item() {
                    self.ensure_line_break();
                } else {
                    self.ensure_blank_line();
                }

                let list = match start {
                    Some(next_index) => ListContext::Ordered {
                        next_index: next_index.max(1),
                        has_items: false,
                    },
                    None => ListContext::Unordered { has_items: false },
                };
                self.list_stack.push(list);
            }
            Tag::Item => {
                let had_items = self
                    .list_stack
                    .last_mut()
                    .map(ListContext::mark_item_started)
                    .unwrap_or(false);
                self.trim_trailing_newlines_to(if had_items { 1 } else { 2 });
                if !self.output.is_empty() && !self.output.ends_with('\n') {
                    self.push_text("\n");
                }
                self.list_item_depth += 1;
                self.push_list_item_prefix();
            }
            Tag::Emphasis => self.open_entity(MessageEntityKind::Italic),
            Tag::Strong => self.open_entity(MessageEntityKind::Bold),
            Tag::Strikethrough => self.open_entity(MessageEntityKind::Strikethrough),
            Tag::Link(_, destination, _) | Tag::Image(_, destination, _) => {
                match destination.parse() {
                    Ok(url) => self.open_entity(MessageEntityKind::TextLink { url }),
                    Err(error) => tracing::debug!(
                        %destination,
                        %error,
                        "telegram formatter skipping unsupported link destination"
                    ),
                }
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: Tag<'_>) {
        if self.handle_table_end_tag(&tag) {
            return;
        }

        match tag {
            Tag::Paragraph => self.close_block(),
            Tag::Heading(..) => {
                self.close_entity();
                self.ensure_blank_line();
            }
            Tag::BlockQuote => {
                self.trim_trailing_newlines_to(0);
                self.close_entity();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.ensure_blank_line();
            }
            Tag::CodeBlock(_) => {
                self.close_entity();
                self.close_block();
            }
            Tag::List(_) => {
                self.trim_trailing_newlines_to(if self.in_list_item() { 1 } else { 0 });
                self.list_stack.pop();
                if !self.in_list_item() {
                    self.ensure_blank_line();
                }
            }
            Tag::Item => {
                self.trim_trailing_newlines_to(0);
                self.push_text("\n");
                self.list_item_depth = self.list_item_depth.saturating_sub(1);
            }
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link(..) | Tag::Image(..) => {
                self.close_entity();
            }
            _ => {}
        }
    }

    fn push_inline_code(&mut self, text: &str) {
        self.open_entity(MessageEntityKind::Code);
        self.push_text(text);
        self.close_entity();
    }

    fn open_entity(&mut self, kind: MessageEntityKind) {
        self.open_entities.push(OpenEntity {
            kind,
            offset: self.output_utf16_len,
        });
    }

    fn close_entity(&mut self) {
        let Some(open_entity) = self.open_entities.pop() else {
            return;
        };
        let length = self.output_utf16_len.saturating_sub(open_entity.offset);
        if length == 0 {
            return;
        }
        self.entities.push(MessageEntity::new(
            open_entity.kind,
            open_entity.offset,
            length,
        ));
    }

    fn close_all_entities(&mut self) {
        while !self.open_entities.is_empty() {
            self.close_entity();
        }
    }

    fn push_text(&mut self, text: &str) {
        self.output_utf16_len += utf16_len(text);
        self.output.push_str(text);
    }

    fn push_list_item_prefix(&mut self) {
        let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
        self.push_text(&indent);

        match self.list_stack.last_mut() {
            Some(ListContext::Ordered { next_index, .. }) => {
                let current = *next_index;
                *next_index += 1;
                self.push_text(&format!("{current}. "));
            }
            Some(ListContext::Unordered { .. }) | None => self.push_text("• "),
        }
    }

    fn in_list_item(&self) -> bool {
        self.list_item_depth > 0
    }

    fn in_table(&self) -> bool {
        self.table_state.is_some()
    }

    fn push_table_text(&mut self, text: &str) -> bool {
        if let Some(table_state) = self.table_state.as_mut() {
            table_state.push_cell_text(text);
            true
        } else {
            false
        }
    }

    fn push_table_footnote(&mut self, reference: &str) -> bool {
        if let Some(table_state) = self.table_state.as_mut() {
            table_state.push_cell_text("[");
            table_state.push_cell_text(reference);
            table_state.push_cell_text("]");
            true
        } else {
            false
        }
    }

    fn handle_table_start_tag(&mut self, tag: &Tag<'_>) -> bool {
        match tag {
            Tag::Table(_) => {
                self.ensure_blank_line();
                self.table_state = Some(TableState::default());
                true
            }
            Tag::TableHead => {
                if let Some(table_state) = self.table_state.as_mut() {
                    table_state.in_header = true;
                    table_state.start_row();
                    true
                } else {
                    false
                }
            }
            Tag::TableRow => {
                if let Some(table_state) = self.table_state.as_mut() {
                    table_state.start_row();
                    true
                } else {
                    false
                }
            }
            Tag::TableCell => {
                if let Some(table_state) = self.table_state.as_mut() {
                    table_state.start_cell();
                    true
                } else {
                    false
                }
            }
            _ => self.in_table(),
        }
    }

    fn handle_table_end_tag(&mut self, tag: &Tag<'_>) -> bool {
        match tag {
            Tag::TableCell => {
                if let Some(table_state) = self.table_state.as_mut() {
                    table_state.finish_cell();
                    true
                } else {
                    false
                }
            }
            Tag::TableRow => {
                if let Some(table_state) = self.table_state.as_mut() {
                    table_state.finish_row();
                    true
                } else {
                    false
                }
            }
            Tag::TableHead => {
                if let Some(table_state) = self.table_state.as_mut() {
                    table_state.finish_row();
                    table_state.in_header = false;
                    true
                } else {
                    false
                }
            }
            Tag::Table(_) => {
                if let Some(table_state) = self.table_state.take() {
                    self.render_table(table_state);
                    self.ensure_blank_line();
                    true
                } else {
                    false
                }
            }
            _ => self.in_table(),
        }
    }

    fn render_table(&mut self, table_state: TableState) {
        let rendered_rows = render_table_rows(&table_state.headers, &table_state.rows);
        if rendered_rows.is_empty() {
            return;
        }

        if !self.output.is_empty() && !self.output.ends_with('\n') {
            self.push_text("\n\n");
        }
        self.push_text(&rendered_rows.join("\n"));
    }

    fn close_block(&mut self) {
        if self.in_list_item() {
            self.ensure_line_break();
        } else {
            self.ensure_blank_line();
        }
    }

    fn ensure_line_break(&mut self) {
        if self.output.is_empty() || self.output.ends_with('\n') {
            return;
        }
        self.push_text("\n");
    }

    fn ensure_blank_line(&mut self) {
        if self.output.is_empty() {
            return;
        }

        let trailing_newlines = self
            .output
            .chars()
            .rev()
            .take_while(|character| *character == '\n')
            .count();
        if trailing_newlines == 0 {
            self.push_text("\n\n");
        } else if trailing_newlines == 1 {
            self.push_text("\n");
        }
    }

    fn trim_trailing_newlines_to(&mut self, max_newlines: usize) {
        let trailing_newlines = self
            .output
            .chars()
            .rev()
            .take_while(|character| *character == '\n')
            .count();
        if trailing_newlines <= max_newlines {
            return;
        }

        let new_len = self.output.trim_end_matches('\n').len() + max_newlines;
        self.output.truncate(new_len);
        self.output_utf16_len = utf16_len(&self.output);
    }
}

fn code_block_language<'a>(kind: &'a CodeBlockKind<'a>) -> Option<&'a str> {
    match kind {
        CodeBlockKind::Indented => None,
        CodeBlockKind::Fenced(language) => language
            .split_whitespace()
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    }
}

fn normalize_table_cell(cell: &str) -> String {
    cell.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_table_rows(headers: &[String], rows: &[Vec<String>]) -> Vec<String> {
    let mut rendered_rows = Vec::new();

    for row in rows {
        let non_empty_cells: Vec<&str> = row
            .iter()
            .map(String::as_str)
            .filter(|cell| !cell.is_empty())
            .collect();
        if non_empty_cells.is_empty() {
            continue;
        }

        if headers.len() == 2 && row.len() >= 2 && !row[0].is_empty() && !row[1].is_empty() {
            rendered_rows.push(format!("• {}: {}", row[0], row[1]));
            continue;
        }

        let labeled_cells: Vec<String> = headers
            .iter()
            .zip(row.iter())
            .filter_map(|(header, cell)| {
                if header.is_empty() || cell.is_empty() {
                    None
                } else {
                    Some(format!("{header}: {cell}"))
                }
            })
            .collect();

        if !labeled_cells.is_empty() {
            rendered_rows.push(format!("• {}", labeled_cells.join("; ")));
        } else {
            rendered_rows.push(format!("• {}", non_empty_cells.join(" | ")));
        }
    }

    rendered_rows
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn render_markdown_to_telegram_entities(
    markdown: &str,
) -> (String, RenderedTelegramText, TelegramRenderMode) {
    let normalized = normalize_telegram_markdown(markdown);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        TelegramEntityRenderer::render(&normalized)
    })) {
        Ok(rendered) => (normalized, rendered, TelegramRenderMode::Entities),
        Err(payload) => {
            tracing::error!(
                panic = %panic_payload_summary(payload.as_ref()),
                "telegram formatter panicked, falling back to plain text"
            );
            let rendered = RenderedTelegramText {
                text: normalized.clone(),
                entities: Vec::new(),
            };
            (normalized, rendered, TelegramRenderMode::PlainTextFallback)
        }
    }
}

fn markdown_to_telegram_entities(markdown: &str) -> RenderedTelegramText {
    let (_, rendered, _) = render_markdown_to_telegram_entities(markdown);
    rendered
}

fn entity_trace(entity: &MessageEntity) -> TelegramEntityTrace {
    let (kind, url, language) = match &entity.kind {
        MessageEntityKind::Mention => ("mention".to_string(), None, None),
        MessageEntityKind::Hashtag => ("hashtag".to_string(), None, None),
        MessageEntityKind::Cashtag => ("cashtag".to_string(), None, None),
        MessageEntityKind::BotCommand => ("bot_command".to_string(), None, None),
        MessageEntityKind::Url => ("url".to_string(), None, None),
        MessageEntityKind::Email => ("email".to_string(), None, None),
        MessageEntityKind::PhoneNumber => ("phone_number".to_string(), None, None),
        MessageEntityKind::Bold => ("bold".to_string(), None, None),
        MessageEntityKind::Blockquote => ("blockquote".to_string(), None, None),
        MessageEntityKind::ExpandableBlockquote => {
            ("expandable_blockquote".to_string(), None, None)
        }
        MessageEntityKind::Italic => ("italic".to_string(), None, None),
        MessageEntityKind::Underline => ("underline".to_string(), None, None),
        MessageEntityKind::Strikethrough => ("strikethrough".to_string(), None, None),
        MessageEntityKind::Spoiler => ("spoiler".to_string(), None, None),
        MessageEntityKind::Code => ("code".to_string(), None, None),
        MessageEntityKind::Pre { language } => ("pre".to_string(), None, language.clone()),
        MessageEntityKind::TextLink { url } => {
            ("text_link".to_string(), Some(url.to_string()), None)
        }
        MessageEntityKind::TextMention { .. } => ("text_mention".to_string(), None, None),
        MessageEntityKind::CustomEmoji { custom_emoji_id } => (
            "custom_emoji".to_string(),
            Some(custom_emoji_id.to_string()),
            None,
        ),
    };

    TelegramEntityTrace {
        kind,
        offset: entity.offset,
        length: entity.length,
        url,
        language,
    }
}

fn rendered_chunk_trace(rendered: RenderedTelegramText) -> TelegramRenderedChunkTrace {
    TelegramRenderedChunkTrace {
        text: rendered.text,
        entities: rendered.entities.iter().map(entity_trace).collect(),
    }
}

pub(crate) fn render_trace(markdown: &str) -> TelegramRenderTrace {
    let (normalized, rendered, mode) = render_markdown_to_telegram_entities(markdown);
    let chunks = split_rendered_text(rendered.clone(), MAX_MESSAGE_LENGTH)
        .into_iter()
        .map(rendered_chunk_trace)
        .collect();

    TelegramRenderTrace {
        input: markdown.to_string(),
        normalized,
        mode,
        rendered: rendered_chunk_trace(rendered),
        chunks,
    }
}

fn panic_payload_summary(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn split_rendered_text(
    rendered: RenderedTelegramText,
    max_chars: usize,
) -> Vec<RenderedTelegramText> {
    if rendered.char_count() <= max_chars {
        return vec![rendered];
    }

    let mut chunks = Vec::new();
    let mut remaining = rendered;

    loop {
        if remaining.char_count() <= max_chars {
            chunks.push(remaining);
            break;
        }

        let (left, right) = split_rendered_once(remaining, max_chars);
        chunks.push(left);
        remaining = right;
    }

    chunks
}

fn split_rendered_once(
    rendered: RenderedTelegramText,
    max_chars: usize,
) -> (RenderedTelegramText, RenderedTelegramText) {
    let hard_split = byte_index_after_n_chars(&rendered.text, max_chars);
    let preferred_split = preferred_split_byte(&rendered.text, hard_split);
    let safe_split = adjust_split_byte_to_entity_boundary(&rendered, preferred_split);
    let split_byte = if safe_split == 0 {
        hard_split
    } else {
        safe_split
    };

    let left_end = trim_trailing_split_whitespace(&rendered.text, split_byte);
    let right_start = trim_leading_split_whitespace(&rendered.text, split_byte);

    let left_end = if left_end == 0 { split_byte } else { left_end };
    let right_start = if right_start >= rendered.text.len() {
        split_byte
    } else {
        right_start
    };

    let left = slice_rendered_text(&rendered, 0, left_end);
    let right = slice_rendered_text(&rendered, right_start, rendered.text.len());

    if left.text.is_empty() || right.text.is_empty() {
        (
            slice_rendered_text(&rendered, 0, hard_split),
            slice_rendered_text(&rendered, hard_split, rendered.text.len()),
        )
    } else {
        (left, right)
    }
}

fn byte_index_after_n_chars(text: &str, max_chars: usize) -> usize {
    let mut count = 0;
    for (byte_index, character) in text.char_indices() {
        if count == max_chars {
            return byte_index;
        }
        count += 1;
        if count == max_chars {
            return byte_index + character.len_utf8();
        }
    }
    text.len()
}

fn preferred_split_byte(text: &str, hard_split: usize) -> usize {
    text[..hard_split]
        .rfind('\n')
        .or_else(|| text[..hard_split].rfind(' '))
        .unwrap_or(hard_split)
}

fn adjust_split_byte_to_entity_boundary(
    rendered: &RenderedTelegramText,
    split_byte: usize,
) -> usize {
    let parsed_entities = MessageEntityRef::parse(&rendered.text, &rendered.entities);
    let mut adjusted = split_byte;

    loop {
        let mut next = adjusted;
        for entity in &parsed_entities {
            if entity.start() < adjusted && adjusted < entity.end() {
                next = next.min(entity.start());
            }
        }

        if next == adjusted {
            return adjusted;
        }

        adjusted = next;
        if adjusted == 0 {
            return 0;
        }
    }
}

fn trim_trailing_split_whitespace(text: &str, split_byte: usize) -> usize {
    let mut end = split_byte;
    while end > 0 {
        let Some(character) = text[..end].chars().next_back() else {
            break;
        };
        if !character.is_whitespace() {
            break;
        }
        end -= character.len_utf8();
    }
    end
}

fn trim_leading_split_whitespace(text: &str, split_byte: usize) -> usize {
    let mut start = split_byte;
    while start < text.len() {
        let Some(character) = text[start..].chars().next() else {
            break;
        };
        if !character.is_whitespace() {
            break;
        }
        start += character.len_utf8();
    }
    start
}

fn slice_rendered_text(
    rendered: &RenderedTelegramText,
    start_byte: usize,
    end_byte: usize,
) -> RenderedTelegramText {
    let text = rendered.text[start_byte..end_byte].to_string();
    let parsed_entities = MessageEntityRef::parse(&rendered.text, &rendered.entities);
    let mut entities = Vec::new();

    for entity in parsed_entities {
        if entity.start() < start_byte || entity.end() > end_byte {
            continue;
        }

        let offset = utf16_len(&rendered.text[start_byte..entity.start()]);
        let length = utf16_len(entity.text());
        entities.push(MessageEntity::new(entity.kind().clone(), offset, length));
    }

    entities.sort_by(|left, right| {
        left.offset
            .cmp(&right.offset)
            .then(right.length.cmp(&left.length))
    });

    RenderedTelegramText { text, entities }
}

/// Render entity-based Telegram output back into HTML for formatter tests.
#[cfg(test)]
fn markdown_to_telegram_html(markdown: &str) -> String {
    let rendered = markdown_to_telegram_entities(markdown);
    render_entities_as_html_preview(&rendered)
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct HtmlPreviewTag {
    byte_offset: usize,
    kind: HtmlPreviewTagKind,
    text: String,
    span_len: usize,
    precedence: usize,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HtmlPreviewTagKind {
    Start,
    End,
}

#[cfg(test)]
fn render_entities_as_html_preview(rendered: &RenderedTelegramText) -> String {
    let parsed_entities = MessageEntityRef::parse(&rendered.text, &rendered.entities);
    let mut tags = Vec::with_capacity(parsed_entities.len() * 2);

    for entity in parsed_entities {
        let (start_tag, end_tag, precedence) = html_preview_tags(entity.kind());
        let span_len = entity.range().len();
        tags.push(HtmlPreviewTag {
            byte_offset: entity.start(),
            kind: HtmlPreviewTagKind::Start,
            text: start_tag,
            span_len,
            precedence,
        });
        tags.push(HtmlPreviewTag {
            byte_offset: entity.end(),
            kind: HtmlPreviewTagKind::End,
            text: end_tag,
            span_len,
            precedence,
        });
    }

    tags.sort_by(|left, right| {
        left.byte_offset
            .cmp(&right.byte_offset)
            .then_with(|| match (left.kind, right.kind) {
                (HtmlPreviewTagKind::End, HtmlPreviewTagKind::Start) => std::cmp::Ordering::Less,
                (HtmlPreviewTagKind::Start, HtmlPreviewTagKind::End) => std::cmp::Ordering::Greater,
                (HtmlPreviewTagKind::Start, HtmlPreviewTagKind::Start) => right
                    .span_len
                    .cmp(&left.span_len)
                    .then(left.precedence.cmp(&right.precedence)),
                (HtmlPreviewTagKind::End, HtmlPreviewTagKind::End) => left
                    .span_len
                    .cmp(&right.span_len)
                    .then(right.precedence.cmp(&left.precedence)),
            })
            .then_with(|| left.text.cmp(&right.text))
    });

    let mut html = String::with_capacity(rendered.text.len() + tags.len() * 8);
    let mut tag_index = 0;

    for (byte_offset, character) in rendered.text.char_indices() {
        while let Some(tag) = tags
            .get(tag_index)
            .filter(|tag| tag.byte_offset == byte_offset)
        {
            html.push_str(&tag.text);
            tag_index += 1;
        }
        push_escaped_html_character(character, &mut html);
    }

    while let Some(tag) = tags.get(tag_index) {
        html.push_str(&tag.text);
        tag_index += 1;
    }

    html
}

#[cfg(test)]
fn html_preview_tags(kind: &MessageEntityKind) -> (String, String, usize) {
    match kind {
        MessageEntityKind::Bold => ("<b>".into(), "</b>".into(), 20),
        MessageEntityKind::Italic => ("<i>".into(), "</i>".into(), 10),
        MessageEntityKind::Underline => ("<u>".into(), "</u>".into(), 30),
        MessageEntityKind::Strikethrough => ("<s>".into(), "</s>".into(), 40),
        MessageEntityKind::Spoiler => ("<tg-spoiler>".into(), "</tg-spoiler>".into(), 50),
        MessageEntityKind::Code => ("<code>".into(), "</code>".into(), 60),
        MessageEntityKind::Pre { language } => {
            let start_tag = match language {
                Some(language) => {
                    format!(
                        "<pre><code class=\"language-{}\">",
                        escape_html_attribute(language)
                    )
                }
                None => "<pre>".into(),
            };
            let end_tag = if language.is_some() {
                "</code></pre>".into()
            } else {
                "</pre>".into()
            };
            (start_tag, end_tag, 70)
        }
        MessageEntityKind::TextLink { url } => (
            format!("<a href=\"{}\">", escape_html_attribute(url.as_str())),
            "</a>".into(),
            80,
        ),
        MessageEntityKind::Blockquote | MessageEntityKind::ExpandableBlockquote => {
            ("<blockquote>".into(), "</blockquote>".into(), 90)
        }
        _ => (String::new(), String::new(), 0),
    }
}

#[cfg(test)]
fn push_escaped_html_character(character: char, html: &mut String) {
    match character {
        '&' => html.push_str("&amp;"),
        '<' => html.push_str("&lt;"),
        '>' => html.push_str("&gt;"),
        _ => html.push(character),
    }
}

#[cfg(test)]
fn escape_html_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Send a plain text Telegram message for formatting fallback paths.
async fn send_plain_text(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    reply_to: Option<MessageId>,
) -> anyhow::Result<()> {
    let mut request = bot.send_message(chat_id, text);
    if let Some(reply_id) = reply_to {
        request = request.reply_parameters(ReplyParameters::new(reply_id));
    }
    request
        .send()
        .await
        .context("failed to send telegram message")?;
    Ok(())
}

/// Send a message using Telegram entities, splitting at the message length
/// limit. Falls back to plain text if Telegram rejects the entity payload.
async fn send_formatted(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    reply_to: Option<MessageId>,
) -> anyhow::Result<()> {
    let rendered = markdown_to_telegram_entities(text);
    for formatted_chunk in split_rendered_text(rendered, MAX_MESSAGE_LENGTH) {
        let mut request = bot.send_message(chat_id, &formatted_chunk.text);
        if !formatted_chunk.entities.is_empty() {
            request = request.entities(formatted_chunk.entities.clone());
        }
        if let Some(reply_id) = reply_to {
            request = request.reply_parameters(ReplyParameters::new(reply_id));
        }
        if let Err(error) = request.send().await {
            tracing::debug!(%error, "entity send failed, retrying as plain text");
            send_plain_text(bot, chat_id, &formatted_chunk.text, reply_to).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold() {
        assert_eq!(
            markdown_to_telegram_html("**bold text**"),
            "<b>bold text</b>"
        );
    }

    #[test]
    fn bold_entities_preserve_plain_text_offsets() {
        let rendered = markdown_to_telegram_entities("**bold text**");
        assert_eq!(rendered.text, "bold text");
        assert_eq!(
            rendered.entities,
            vec![MessageEntity::bold(0, utf16_len("bold text"))]
        );
    }

    #[test]
    fn split_rendered_text_keeps_entities_on_safe_boundaries() {
        let rendered = markdown_to_telegram_entities("One two **three** four");
        let chunks = split_rendered_text(rendered, 10);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "One two");
        assert!(chunks[0].entities.is_empty());
        assert_eq!(chunks[1].text, "three four");
        assert_eq!(chunks[1].entities, vec![MessageEntity::bold(0, 5)]);
    }

    #[test]
    fn split_rendered_text_rebases_utf16_offsets() {
        let rendered = markdown_to_telegram_entities("Prefix **🐧 alpha** suffix");
        let chunks = split_rendered_text(rendered, 8);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].text, "🐧 alpha");
        assert_eq!(
            chunks[1].entities,
            vec![MessageEntity::bold(0, utf16_len("🐧 alpha"))]
        );
    }

    #[test]
    fn italic() {
        assert_eq!(
            markdown_to_telegram_html("*italic text*"),
            "<i>italic text</i>"
        );
    }

    #[test]
    fn bold_with_underscores() {
        assert_eq!(
            markdown_to_telegram_html("__bold text__"),
            "<b>bold text</b>"
        );
    }

    #[test]
    fn italic_with_underscores() {
        assert_eq!(
            markdown_to_telegram_html("_italic text_"),
            "<i>italic text</i>"
        );
    }

    #[test]
    fn bold_and_italic_nested() {
        assert_eq!(
            markdown_to_telegram_html("***both***"),
            "<i><b>both</b></i>"
        );
    }

    #[test]
    fn inline_code() {
        assert_eq!(
            markdown_to_telegram_html("use `println!` here"),
            "use <code>println!</code> here"
        );
    }

    #[test]
    fn code_block_with_language() {
        let input = "```rust\nfn main() {}\n```";
        let expected = "<pre><code class=\"language-rust\">fn main() {}</code></pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn code_block_without_language() {
        let input = "```\nhello world\n```";
        let expected = "<pre>hello world</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn code_block_escapes_html() {
        let input = "```\n<script>alert(1)</script>\n```";
        let expected = "<pre>&lt;script&gt;alert(1)&lt;/script&gt;</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn link() {
        assert_eq!(
            markdown_to_telegram_html("[click](https://example.com)"),
            r#"<a href="https://example.com/">click</a>"#
        );
    }

    #[test]
    fn relative_link_falls_back_to_plain_text() {
        let rendered = markdown_to_telegram_entities("[docs](docs/tasks)");
        assert_eq!(rendered.text, "docs");
        assert!(rendered.entities.is_empty());
    }

    #[test]
    fn relative_image_falls_back_to_alt_text() {
        let rendered = markdown_to_telegram_entities("![diagram](docs/diagram.png)");
        assert_eq!(rendered.text, "diagram");
        assert!(rendered.entities.is_empty());
    }

    #[test]
    fn strikethrough() {
        assert_eq!(markdown_to_telegram_html("~~deleted~~"), "<s>deleted</s>");
    }

    #[test]
    fn headers_render_as_bold() {
        assert_eq!(markdown_to_telegram_html("# Title"), "<b>Title</b>");
        assert_eq!(markdown_to_telegram_html("## Sub"), "<b>Sub</b>");
        assert_eq!(markdown_to_telegram_html("### Section"), "<b>Section</b>");
    }

    #[test]
    fn blockquote() {
        assert_eq!(
            markdown_to_telegram_html("> quoted text"),
            "<blockquote>quoted text</blockquote>"
        );
    }

    #[test]
    fn multiline_blockquote() {
        let input = "> line one\n> line two";
        let expected = "<blockquote>line one\nline two</blockquote>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn blockquote_then_list_keeps_structure() {
        let input = "> Summary\n\n- Memory store: none\n- Inbox: clear";
        let expected = "<blockquote>Summary</blockquote>\n\n• Memory store: none\n• Inbox: clear";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn html_entities_escaped_in_text() {
        assert_eq!(
            markdown_to_telegram_html("x < y & a > b"),
            "x &lt; y &amp; a &gt; b"
        );
    }

    #[test]
    fn inline_code_escapes_html() {
        assert_eq!(
            markdown_to_telegram_html("`<b>not bold</b>`"),
            "<code>&lt;b&gt;not bold&lt;/b&gt;</code>"
        );
    }

    #[test]
    fn mixed_formatting() {
        let input = "Hello **world**, this is *important* and `code`";
        let expected = "Hello <b>world</b>, this is <i>important</i> and <code>code</code>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(
            markdown_to_telegram_html("just plain text"),
            "just plain text"
        );
    }

    #[test]
    fn unclosed_code_block_runs_to_eof() {
        let input = "```python\nprint('hi')";
        let expected = "<pre><code class=\"language-python\">print('hi')</code></pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn strip_html_tags_and_unescape() {
        assert_eq!(
            strip_html_tags("<b>bold</b> &amp; <i>italic</i>"),
            "bold & italic"
        );
    }

    #[test]
    fn unordered_lists_render_as_bullets() {
        let input = "- item one\n- item two\n- item three";
        let expected = "• item one\n• item two\n• item three";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn ordered_lists_preserve_numbers() {
        let input = "1. first\n2. second\n3. third";
        let expected = "1. first\n2. second\n3. third";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn task_lists_render_as_checkboxes() {
        let input = "- [x] done\n- [ ] next";
        let expected = "• [x] done\n• [ ] next";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn nested_lists_indent_children() {
        let input = "- parent\n  - child";
        let expected = "• parent\n  • child";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn raw_html_is_escaped() {
        let input = "<b>not actually bold</b>";
        let expected = "&lt;b&gt;not actually bold&lt;/b&gt;";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn raw_html_stays_literal_in_entity_text() {
        let rendered = markdown_to_telegram_entities("<b>not actually bold</b>");
        assert_eq!(rendered.text, "<b>not actually bold</b>");
        assert!(rendered.entities.is_empty());
    }

    #[test]
    fn normalizes_collapsed_numbered_lists() {
        let input = "To finish setup, do this:1. **Open the control panel** to confirm the current state2. **Copy the access token** into the dashboard3. **Check your local notes** for the next follow-up";
        let expected = "To finish setup, do this:\n\n1. <b>Open the control panel</b> to confirm the current state\n2. <b>Copy the access token</b> into the dashboard\n3. <b>Check your local notes</b> for the next follow-up";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_collapsed_bullet_lists() {
        let input = "A few possibilities:- The message might be filed in another folder- It could already be archived";
        let expected = "A few possibilities:\n\n• The message might be filed in another folder\n• It could already be archived";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_common_prose_spacing() {
        let input = "The update was posted today (April9,2026) at7:45 PM. You'll need to review it within the last30 days.";
        let expected = "The update was posted today (April 9, 2026) at 7:45 PM. You'll need to review it within the last 30 days.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn preserves_slash_delimited_timezones() {
        let input = "It's **11:16 AM** (Asia/Singapore time) on March13,2026.";
        let expected = "It's **11:16 AM** (Asia/Singapore time) on March 13, 2026.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn normalizes_ordinal_spacing() {
        let input = "The only pending item is scheduled for Wednesday the11th.";
        let expected = "The only pending item is scheduled for Wednesday the 11th.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn normalizes_glued_sentence_starters_after_non_alpha_tokens() {
        let input = "Reference page: https://example.com/r/A1b2C3d4X9Then the review started.";
        let expected = "Reference page: https://example.com/r/A1b2C3d4X9 Then the review started.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn normalizes_lowercase_word_number_spacing() {
        let input = "See questions13–15 before launch and within the last30 days.";
        let expected = "See questions 13–15 before launch and within the last 30 days.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn normalizes_glued_section_labels_after_tokens() {
        let input = "Summary: Re: Weekly rollout statusAction items:- Added the fallback path";
        let expected =
            "Summary: Re: Weekly rollout status\n\nAction items:\n\n• Added the fallback path";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn leaves_inline_prose_labels_without_block_followups_alone() {
        let input =
            "I've sent the comprehensive Markdown document: **guide.md**The guide continues below.";
        let expected = "I've sent the comprehensive Markdown document: <b>guide.md</b> The guide continues below.";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_list_item_tail_before_next_emphasized_heading() {
        let input = "Summary:\n- March13,1:00-3:00 PM**Task #10**: Meeting with Azizul";
        let expected = "Summary:\n• March 13, 1:00-3:00 PM\n\n<b>Task #10</b>: Meeting with Azizul";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_list_item_tail_before_next_sentence() {
        let input = "Summary:\n- implementation stepsThe worker will deliver the final document.";
        let expected =
            "Summary:\n• implementation steps\n\nThe worker will deliver the final document.";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_dense_task_board_report() {
        let input = "Here's the current summary:**Pending Review (3 items)**1. **#7: Prepare rollout checklist** (High Priority) - Status: waiting_review - Deadline: April18,2026 - Next: Needs approval to proceed2. **#5: Draft launch materials** (High Priority) - Deadline: April18-19,2026 - Next: Needs approval to begin writing3. **#1: Reconcile support backlog** (Medium Priority) - Next: Needs approval to contact ops**Completed / In Progress**- **#6: Send update note** — Partially completeThe3 pending items need approval before they can move forward.";
        let expected = "Here's the current summary:\n\n**Pending Review (3 items)**\n1. **#7: Prepare rollout checklist** (High Priority)\n- Status: waiting_review\n- Deadline: April 18, 2026\n- Next: Needs approval to proceed\n2. **#5: Draft launch materials** (High Priority)\n- Deadline: April 18-19, 2026\n- Next: Needs approval to begin writing\n3. **#1: Reconcile support backlog** (Medium Priority)\n- Next: Needs approval to contact ops\n**Completed / In Progress**\n- **#6: Send update note** — Partially complete\nThe 3 pending items need approval before they can move forward.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn normalizes_dense_gap_report() {
        let input = "The item was marked \"done\" but it was not actually completed. Here's the review:**Open issues:**- Item #3 shows \"done\" (marked April11,1:17 AM)- But there's no follow-up action sent- No notes reviewed- No action items documented- No next checkpoint scheduled- The subtasks under #3 all show `completed: false`Basically it was closed without the work being finished.";
        let expected = "The item was marked \"done\" but it was not actually completed. Here's the review:\n\n**Open issues:**\n- Item #3 shows \"done\" (marked April 11, 1:17 AM)\n- But there's no follow-up action sent\n- No notes reviewed\n- No action items documented\n- No next checkpoint scheduled\n- The subtasks under #3 all show `completed: false` Basically it was closed without the work being finished.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn inline_code_spans_are_not_normalized() {
        let input = "Keep `item13-15` and `April12` literal, but outside code April12 at8:00 AM should be readable.";
        let expected = "Keep <code>item13-15</code> and <code>April12</code> literal, but outside code April 12 at 8:00 AM should be readable.";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn inserts_space_after_inline_code_before_titlecase_prose() {
        let input = "The checklist entry shows `completed: false`Basically it stayed unresolved.";
        let expected =
            "The checklist entry shows `completed: false` Basically it stayed unresolved.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn all_caps_model_like_tokens_are_left_alone() {
        let input =
            "RTX3080, GPT4, and Qwen3.5 should stay compact while The3 tasks should be readable.";
        let expected =
            "RTX3080, GPT4, and Qwen3.5 should stay compact while The 3 tasks should be readable.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn table_rows_flatten_to_bullets() {
        let input = "| Scenario | Effect |\n| --- | --- |\n| Abrupt war end | Spike to 108-110 |\n| Prolonged conflict | Sustained 106-112 |";
        let expected =
            "• Abrupt war end: Spike to 108-110\n• Prolonged conflict: Sustained 106-112";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn multi_column_table_rows_flatten_to_label_value_bullets() {
        let input = "| Region | Impact | Risk |\n| --- | --- | --- |\n| Singapore | Growth slows | Medium |";
        let expected = "• Region: Singapore; Impact: Growth slows; Risk: Medium";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_collapsed_bullet_list_after_bold_heading() {
        let input = "**Safe-Haven Flows**- USD strengthens\n- Treasuries rally";
        let expected = "<b>Safe-Haven Flows</b>\n\n• USD strengthens\n• Treasuries rally";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_emphasized_heading_and_list_after_punctuation() {
        let input = "Here's what I found:**What I Found:**- First point\n- Second point";
        let expected =
            "Here's what I found:\n\n<b>What I Found:</b>\n\n• First point\n• Second point";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_emphasized_field_runs_into_label_lines() {
        let input = "Today's update:**From:** Alex Example<alex@example.com>**Subject:** RE: Status update**Time:**8:51 AM SGT";
        let expected = "Today's update:\n\n<b>From:</b> Alex Example&lt;alex@example.com&gt;\n<b>Subject:</b> RE: Status update\n<b>Time:</b> 8:51 AM SGT";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn normalizes_emphasized_content_label_before_blockquote() {
        let input = "**Content:**> Line one\n> Line two";
        let expected = "<b>Content:</b>\n<blockquote>Line one\nLine two</blockquote>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn leaves_inline_bold_phrases_with_spaces_alone() {
        let input = "This is **very important** today, but it is still part of one sentence.";
        let expected = "This is **very important** today, but it is still part of one sentence.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn preserves_mixed_case_product_names_and_acronyms() {
        let input = "The worker will research ServiceNow integrations, free GitHub tooling, and direct REST APIs.";
        assert_eq!(normalize_telegram_markdown(input), input);
    }

    #[test]
    fn retries_plain_caption_only_for_parse_entity_errors() {
        let parse_error = RequestError::Api(ApiError::CantParseEntities(
            "Bad Request: can't parse entities".into(),
        ));
        let non_parse_error = RequestError::Api(ApiError::BotBlocked);

        assert!(should_retry_plain_caption(&parse_error));
        assert!(!should_retry_plain_caption(&non_parse_error));
    }
}

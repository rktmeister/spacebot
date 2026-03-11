//! Telegram messaging adapter using teloxide.

use crate::config::TelegramPermissions;
use crate::messaging::apply_runtime_adapter_to_conversation_id;
use crate::messaging::traits::{InboundStream, Messaging};
use crate::{Attachment, InboundMessage, MessageContent, OutboundResponse, StatusUpdate};

use anyhow::Context as _;
use arc_swap::ArcSwap;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use regex::Regex;
use teloxide::payloads::setters::*;
use teloxide::requests::{Request, Requester};
use teloxide::types::{
    ChatAction, ChatId, FileId, InputFile, InputPollOption, MediaKind, MessageId, MessageKind,
    ParseMode, ReactionType, ReplyParameters, UpdateKind, UserId,
};
use teloxide::{ApiError, Bot, RequestError};

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

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

/// Smaller source-chunk target for markdown that expands heavily when HTML-escaped.
const FORMATTED_SPLIT_LENGTH: usize = MAX_MESSAGE_LENGTH / 2;

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
                        let html_caption = markdown_to_telegram_html(caption_text);
                        self.bot
                            .send_audio(chat_id, input_file)
                            .caption(&html_caption)
                            .parse_mode(ParseMode::Html)
                            .send()
                            .await
                    } else {
                        self.bot.send_audio(chat_id, input_file).send().await
                    };

                    if let Err(error) = sent {
                        if should_retry_plain_caption(&error) {
                            tracing::debug!(
                                %error,
                                "HTML caption parse failed, retrying telegram audio with plain caption"
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
                            return Err(error)
                                .context("failed to send telegram audio with HTML caption")?;
                        }
                    }
                } else {
                    let input_file = InputFile::memory(data.clone()).file_name(filename.clone());
                    let sent = if let Some(ref caption_text) = caption {
                        let html_caption = markdown_to_telegram_html(caption_text);
                        self.bot
                            .send_document(chat_id, input_file)
                            .caption(&html_caption)
                            .parse_mode(ParseMode::Html)
                            .send()
                            .await
                    } else {
                        self.bot.send_document(chat_id, input_file).send().await
                    };

                    if let Err(error) = sent {
                        if should_retry_plain_caption(&error) {
                            tracing::debug!(
                                %error,
                                "HTML caption parse failed, retrying telegram file with plain caption"
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
                            return Err(error)
                                .context("failed to send telegram file with HTML caption")?;
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

                    let html = markdown_to_telegram_html(&display_text);
                    if let Err(html_error) = self
                        .bot
                        .edit_message_text(stream.chat_id, stream.message_id, &html)
                        .parse_mode(ParseMode::Html)
                        .send()
                        .await
                    {
                        tracing::debug!(%html_error, "HTML edit failed, retrying as plain text");
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

/// Split a message into chunks that fit within Telegram's character limit.
/// Tries to split at newlines, then spaces, then hard-cuts.
fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }

        let split_at = remaining[..max_len]
            .rfind('\n')
            .or_else(|| remaining[..max_len].rfind(' '))
            .unwrap_or(max_len);

        chunks.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start();
    }

    chunks
}

/// Return true when Telegram rejected rich text entities and a plain-caption retry is safe.
fn should_retry_plain_caption(error: &RequestError) -> bool {
    matches!(error, RequestError::Api(ApiError::CantParseEntities(_)))
}

// -- Markdown-to-Telegram-HTML formatting --

/// Escape characters that have special meaning in Telegram's HTML parse mode.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape characters that are unsafe in HTML attributes.
fn escape_html_attribute(text: &str) -> String {
    escape_html(text).replace('"', "&quot;")
}

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
    let mut line = normalize_token_boundaries(line);
    line = normalize_prose_spacing(&line);
    line = normalize_inline_list_boundaries(&line);
    line = normalize_emphasized_heading_boundaries(&line);
    line = normalize_list_item_tail_boundaries(&line);
    line
}

/// Repair boundaries that occur immediately after an inline code span before
/// the following plain-text segment is normalized normally.
fn normalize_plain_segment_after_inline_code(segment: &str) -> String {
    static LEADING_SECTION_LABEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?P<label>(?:[A-Z][A-Za-z]+|[A-Z]{2,})(?: [A-Za-z][A-Za-z'/-]+){0,2}:)")
            .expect("hardcoded regex")
    });
    static LEADING_TITLECASE_WORD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?P<word>[A-Z][a-z][A-Za-z']+)").expect("hardcoded regex"));

    let mut segment = LEADING_SECTION_LABEL
        .replace(segment, "\n\n$label")
        .into_owned();
    segment = LEADING_TITLECASE_WORD
        .replace(&segment, " $word")
        .into_owned();
    normalize_plain_markdown_line(&segment)
}

/// Repair compact prose spacing such as `March12`, `at8:00`, `The3`, and
/// `questions13-15` without touching all-caps model/version tokens.
fn normalize_prose_spacing(line: &str) -> String {
    static SENTENCE_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<punctuation>[.!?])(?P<word>[A-Z][A-Za-z']*)").expect("hardcoded regex")
    });
    static MONTH_DAY_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(?P<month>January|February|March|April|May|June|July|August|September|October|November|December)(?P<day>\d{1,2}\b)",
        )
        .expect("hardcoded regex")
    });
    static LETTER_COMMA_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<prefix>[A-Za-z]),(?P<suffix>[A-Za-z0-9])").expect("hardcoded regex")
    });
    static DAY_YEAR_COMMA_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<day>\b\d{1,2}),(?P<year>\d{4}\b)").expect("hardcoded regex")
    });
    static DAY_TIME_COMMA_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<day>\b\d{1,2}),(?P<time>\d{1,2}:\d{2}\b)").expect("hardcoded regex")
    });
    static PREPOSITION_NUMBER_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\b(?P<word>at|by|for|from|in|last|next|on|since|today|tomorrow|yesterday)(?P<number>\d)",
        )
        .expect("hardcoded regex")
    });
    static THE_ORDINAL_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?P<word>the)(?P<ordinal>\d{1,2}(?:st|nd|rd|th)\b)")
            .expect("hardcoded regex")
    });
    static WORD_NUMBER_SPACING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?P<word>\b(?:[A-Z][a-z]{2,}|[a-z]{3,}))(?P<number>\d{1,2}(?::\d{2})?(?:[–-]\d{1,2})?)(?P<after>[^0-9A-Za-z.]|$)",
        )
        .expect("hardcoded regex")
    });

    let mut line = SENTENCE_SPACING
        .replace_all(line, "$punctuation $word")
        .into_owned();
    line = MONTH_DAY_SPACING
        .replace_all(&line, "$month $day")
        .into_owned();
    line = LETTER_COMMA_SPACING
        .replace_all(&line, "$prefix, $suffix")
        .into_owned();
    line = DAY_YEAR_COMMA_SPACING
        .replace_all(&line, "$day, $year")
        .into_owned();
    line = DAY_TIME_COMMA_SPACING
        .replace_all(&line, "$day, $time")
        .into_owned();
    line = PREPOSITION_NUMBER_SPACING
        .replace_all(&line, "$word $number")
        .into_owned();
    line = THE_ORDINAL_SPACING
        .replace_all(&line, "$word $ordinal")
        .into_owned();
    WORD_NUMBER_SPACING
        .replace_all(&line, "$word $number$after")
        .into_owned()
}

/// Repair missing boundaries between a completed token and the next sentence or
/// section label, while keeping the fixes narrow to obvious prose transitions.
fn normalize_token_boundaries(line: &str) -> String {
    static GLUED_URL_AND_TITLECASE_WORD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<url>https?://[^\s<>()]+[A-Za-z0-9])(?P<word>[A-Z][a-z][A-Za-z']+)\b")
            .expect("hardcoded regex")
    });
    static GLUED_TITLECASE_WORD_BEFORE_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<before>[A-Za-z0-9\)\]])(?P<word>[A-Z][a-z]{2,})(?P<number>\d)")
            .expect("hardcoded regex")
    });
    static GLUED_SENTENCE_STARTER_AFTER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?P<before>[A-Za-z0-9\)\]])(?P<word>The|This|That|These|Those|She|He|They|We|You|It|However|Instead|Meanwhile|Also)\b",
        )
        .expect("hardcoded regex")
    });
    static GLUED_SECTION_LABEL_AFTER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?P<before>[A-Za-z0-9\)\]])(?P<label>(?:[A-Z][A-Za-z]+|[A-Z]{2,})(?: [A-Za-z][A-Za-z'/-]+){0,2}:)",
        )
        .expect("hardcoded regex")
    });

    let mut line = GLUED_URL_AND_TITLECASE_WORD
        .replace_all(line, "$url $word")
        .into_owned();
    line = GLUED_TITLECASE_WORD_BEFORE_NUMBER
        .replace_all(&line, "$before $word$number")
        .into_owned();
    line = GLUED_SENTENCE_STARTER_AFTER_TOKEN
        .replace_all(&line, "$before $word")
        .into_owned();
    line = GLUED_SECTION_LABEL_AFTER_TOKEN
        .replace_all(&line, "$before\n\n$label")
        .into_owned();
    line
}

/// Repair compact heading boundaries such as `summary:**Heading**1.` or
/// `summary:**Findings:**- item` before pulldown-cmark parses the markdown.
fn normalize_emphasized_heading_boundaries(line: &str) -> String {
    static TOKEN_BEFORE_EMPHASIZED_HEADING_WITH_LIST_AFTER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?P<before>[A-Za-z0-9\)\]])(?P<heading>(?:\*\*|__)[A-Z][^*\n]* [^*\n]*?(?:\*\*|__))(?P<after>\d+\.|[-*•])",
        )
        .expect("hardcoded regex")
    });

    let line = TOKEN_BEFORE_EMPHASIZED_HEADING_WITH_LIST_AFTER
        .replace_all(line, "$before\n$heading\n$after")
        .into_owned();

    let mut normalized = String::with_capacity(line.len() + line.len() / 8);
    let mut index = 0;

    while index < line.len() {
        if let Some((closing_start, closing_end)) = find_emphasis_span(&line, index) {
            let span = &line[index..closing_end];
            let inner = &line[index + 2..closing_start];
            let previous = previous_non_whitespace_char(&normalized);
            let following = &line[closing_end..];

            if is_block_heading_candidate(inner, &normalized, previous, following) {
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

        let character = line[index..].chars().next().expect("valid utf-8 boundary");
        normalized.push(character);
        index += character.len_utf8();
    }

    normalized
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
    let Some(marker_end) = trimmed.rfind('.') else {
        return false;
    };
    let digits = &trimmed[..marker_end];
    let Some(last_non_digit) = digits.rfind(|character: char| !character.is_ascii_digit()) else {
        return !digits.is_empty();
    };
    last_non_digit + 1 < digits.len()
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

fn starts_compact_list_marker(text: &str) -> bool {
    let trimmed = text.trim_start_matches([' ', '\t']);

    if trimmed.starts_with('-') || trimmed.starts_with('*') || trimmed.starts_with('•') {
        return trimmed[1..].chars().next().is_some_and(|character| {
            character.is_whitespace()
                || character == '`'
                || character == '#'
                || character.is_ascii_alphanumeric()
        });
    }

    let mut digit_length = 0;
    for character in trimmed.chars() {
        if character.is_ascii_digit() {
            digit_length += character.len_utf8();
        } else {
            break;
        }
    }

    if digit_length == 0 || !trimmed[digit_length..].starts_with('.') {
        return false;
    }

    trimmed[digit_length + 1..]
        .chars()
        .next()
        .is_some_and(|character| {
            character.is_whitespace()
                || character == '*'
                || character == '_'
                || character == '`'
                || character == '#'
                || character.is_ascii_alphanumeric()
        })
}

/// Repair inline lists that were emitted mid-sentence instead of on their own
/// lines, but keep normal punctuation and prose intact.
fn normalize_inline_list_boundaries(line: &str) -> String {
    static INLINE_ORDERED_LIST_AFTER_PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<prefix>[:.!?])(?P<gap>[ \t]*)(?P<marker>\d+\.)[ \t]+")
            .expect("hardcoded regex")
    });
    static INLINE_UNORDERED_LIST_AFTER_PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<prefix>[:.!?])(?P<gap>[ \t]*)(?P<marker>[-*•])[ \t]+")
            .expect("hardcoded regex")
    });
    static INLINE_ORDERED_LIST_AFTER_WORD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<before>[A-Za-z0-9)\]])(?P<marker>\d+\.)[ \t]+(?P<next>\*\*|__|`|[A-Z])")
            .expect("hardcoded regex")
    });
    static INLINE_UNORDERED_LIST_AFTER_WORD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<before>[A-Za-z0-9)\]])(?P<marker>[-*•])[ \t]+(?P<next>\*\*|__|`|[A-Z])")
            .expect("hardcoded regex")
    });
    static INLINE_LABEL_BULLET_WITH_SPACES: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?P<before>[a-z0-9\)\]]) (?P<marker>[-•]) (?P<label>(?:[A-Z][A-Za-z'/-]+(?: [A-Za-z][A-Za-z'/-]+){0,2}):)",
        )
        .expect("hardcoded regex")
    });

    let mut line = INLINE_ORDERED_LIST_AFTER_PUNCTUATION
        .replace_all(line, "$prefix\n\n$marker ")
        .into_owned();
    line = INLINE_UNORDERED_LIST_AFTER_PUNCTUATION
        .replace_all(&line, "$prefix\n\n$marker ")
        .into_owned();
    line = INLINE_ORDERED_LIST_AFTER_WORD
        .replace_all(&line, "$before\n$marker $next")
        .into_owned();
    line = INLINE_UNORDERED_LIST_AFTER_WORD
        .replace_all(&line, "$before\n$marker $next")
        .into_owned();
    line = INLINE_LABEL_BULLET_WITH_SPACES
        .replace_all(&line, "$before\n$marker $label")
        .into_owned();
    line
}

/// Repair sentence starts that leak onto the end of a bullet item instead of
/// becoming their own paragraph below the list.
fn normalize_list_item_tail_boundaries(line: &str) -> String {
    static BULLET_ITEM_TAIL_SENTENCE_START: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?m)(?P<item>^[-•].*?[a-z]) (?P<word>The|This|That|These|Those) (?P<number>\d)",
        )
        .expect("hardcoded regex")
    });

    BULLET_ITEM_TAIL_SENTENCE_START
        .replace_all(line, "$item\n$word $number")
        .into_owned()
}

/// Strip HTML tags and unescape entities, producing plain text for fallback.
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

#[derive(Debug)]
struct TelegramHtmlRenderer {
    output: String,
    list_stack: Vec<ListContext>,
    list_item_depth: usize,
    blockquote_depth: usize,
    table_state: Option<TableState>,
}

impl TelegramHtmlRenderer {
    fn new(capacity: usize) -> Self {
        Self {
            output: String::with_capacity(capacity),
            list_stack: Vec::new(),
            list_item_depth: 0,
            blockquote_depth: 0,
            table_state: None,
        }
    }

    fn render(markdown: &str) -> String {
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

    fn finish(mut self) -> String {
        self.trim_trailing_newlines_to(0);
        self.output
    }

    fn push_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) | Event::Html(text) => {
                if self.push_table_text(text.as_ref()) {
                    return;
                }
                self.output.push_str(&escape_html(text.as_ref()));
            }
            Event::Code(text) => {
                if self.push_table_text(text.as_ref()) {
                    return;
                }
                self.output.push_str("<code>");
                self.output.push_str(&escape_html(text.as_ref()));
                self.output.push_str("</code>");
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.push_table_text(" ") {
                    return;
                }
                self.output.push('\n');
            }
            Event::Rule => {
                if self.in_list_item() {
                    self.ensure_line_break();
                } else {
                    self.ensure_blank_line();
                }
                self.output.push_str("──────────");
                self.close_block();
            }
            Event::TaskListMarker(checked) => {
                if self.push_table_text(if checked { "[x] " } else { "[ ] " }) {
                    return;
                }
                self.output.push_str(if checked { "[x] " } else { "[ ] " });
            }
            Event::FootnoteReference(reference) => {
                if self.push_table_footnote(reference.as_ref()) {
                    return;
                }
                self.output.push('[');
                self.output.push_str(&escape_html(reference.as_ref()));
                self.output.push(']');
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
                self.output.push_str("<b>");
            }
            Tag::BlockQuote => {
                self.ensure_blank_line();
                self.output.push_str("<blockquote>");
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                if self.in_list_item() {
                    self.ensure_line_break();
                } else {
                    self.ensure_blank_line();
                }

                if let Some(language) = code_block_language(&kind) {
                    self.output.push_str("<pre><code class=\"language-");
                    self.output.push_str(&escape_html_attribute(language));
                    self.output.push_str("\">");
                } else {
                    self.output.push_str("<pre>");
                }
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
                    self.output.push('\n');
                }
                self.list_item_depth += 1;
                self.push_list_item_prefix();
            }
            Tag::Emphasis => self.output.push_str("<i>"),
            Tag::Strong => self.output.push_str("<b>"),
            Tag::Strikethrough => self.output.push_str("<s>"),
            Tag::Link(_, destination, _) | Tag::Image(_, destination, _) => {
                self.output.push_str("<a href=\"");
                self.output
                    .push_str(&escape_html_attribute(destination.as_ref()));
                self.output.push_str("\">");
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
                self.output.push_str("</b>");
                self.ensure_blank_line();
            }
            Tag::BlockQuote => {
                self.trim_trailing_newlines_to(0);
                self.output.push_str("</blockquote>");
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.ensure_blank_line();
            }
            Tag::CodeBlock(kind) => {
                if code_block_language(&kind).is_some() {
                    self.output.push_str("</code></pre>");
                } else {
                    self.output.push_str("</pre>");
                }
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
                self.output.push('\n');
                self.list_item_depth = self.list_item_depth.saturating_sub(1);
            }
            Tag::Emphasis => self.output.push_str("</i>"),
            Tag::Strong => self.output.push_str("</b>"),
            Tag::Strikethrough => self.output.push_str("</s>"),
            Tag::Link(..) | Tag::Image(..) => self.output.push_str("</a>"),
            _ => {}
        }
    }

    fn push_list_item_prefix(&mut self) {
        let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
        self.output.push_str(&indent);

        match self.list_stack.last_mut() {
            Some(ListContext::Ordered { next_index, .. }) => {
                let current = *next_index;
                *next_index += 1;
                self.output.push_str(&format!("{current}. "));
            }
            Some(ListContext::Unordered { .. }) | None => self.output.push_str("• "),
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
            self.output.push_str("\n\n");
        }
        self.output.push_str(&rendered_rows.join("\n"));
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
        self.output.push('\n');
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
            self.output.push_str("\n\n");
        } else if trailing_newlines == 1 {
            self.output.push('\n');
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

        for _ in 0..(trailing_newlines - max_newlines) {
            self.output.pop();
        }
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

/// Convert markdown to Telegram-compatible HTML.
///
/// Telegram only supports a narrow HTML subset, so markdown is parsed into
/// structure first and then rendered into supported tags plus plain-text list
/// markers and spacing.
fn markdown_to_telegram_html(markdown: &str) -> String {
    TelegramHtmlRenderer::render(&normalize_telegram_markdown(markdown))
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

/// Send a message with Telegram HTML formatting, splitting at the message
/// length limit. Falls back to plain text if the API rejects the HTML.
async fn send_formatted(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    reply_to: Option<MessageId>,
) -> anyhow::Result<()> {
    let mut pending_chunks: VecDeque<String> =
        VecDeque::from(split_message(text, MAX_MESSAGE_LENGTH));
    while let Some(markdown_chunk) = pending_chunks.pop_front() {
        let html_chunk = markdown_to_telegram_html(&markdown_chunk);

        if html_chunk.len() > MAX_MESSAGE_LENGTH {
            let smaller_chunks = split_message(&markdown_chunk, FORMATTED_SPLIT_LENGTH);
            if smaller_chunks.len() > 1 {
                for chunk in smaller_chunks.into_iter().rev() {
                    pending_chunks.push_front(chunk);
                }
                continue;
            }

            let plain_chunk = strip_html_tags(&html_chunk);
            send_plain_text(bot, chat_id, &plain_chunk, reply_to).await?;
            continue;
        }

        let mut request = bot
            .send_message(chat_id, &html_chunk)
            .parse_mode(ParseMode::Html);
        if let Some(reply_id) = reply_to {
            request = request.reply_parameters(ReplyParameters::new(reply_id));
        }
        if let Err(error) = request.send().await {
            tracing::debug!(%error, "HTML send failed, retrying as plain text");
            let plain_chunk = strip_html_tags(&html_chunk);
            send_plain_text(bot, chat_id, &plain_chunk, reply_to).await?;
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
        let expected = "<pre><code class=\"language-rust\">fn main() {}\n</code></pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn code_block_without_language() {
        let input = "```\nhello world\n```";
        let expected = "<pre>hello world\n</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn code_block_escapes_html() {
        let input = "```\n<script>alert(1)</script>\n```";
        let expected = "<pre>&lt;script&gt;alert(1)&lt;/script&gt;\n</pre>";
        assert_eq!(markdown_to_telegram_html(input), expected);
    }

    #[test]
    fn link() {
        assert_eq!(
            markdown_to_telegram_html("[click](https://example.com)"),
            r#"<a href="https://example.com">click</a>"#
        );
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
    fn normalizes_ordinal_spacing() {
        let input = "The only pending item is scheduled for Wednesday the11th.";
        let expected = "The only pending item is scheduled for Wednesday the 11th.";
        assert_eq!(normalize_telegram_markdown(input), expected);
    }

    #[test]
    fn normalizes_glued_sentence_starters_after_tokens() {
        let input = "Reference page: https://example.com/r/A1b2C3d4X9Then the review started. Another reviewer also left notesThe item was opened today.";
        let expected = "Reference page: https://example.com/r/A1b2C3d4X9 Then the review started. Another reviewer also left notes The item was opened today.";
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
    fn leaves_inline_bold_phrases_with_spaces_alone() {
        let input = "This is **very important** today, but it is still part of one sentence.";
        let expected = "This is **very important** today, but it is still part of one sentence.";
        assert_eq!(normalize_telegram_markdown(input), expected);
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

use std::ops::Range;

use threadlane_protocol::daemon::{ChatMessageInfo, MessageRole, ToolActivityInfo};

pub fn is_queued_message(message: &ChatMessageInfo, generating: bool) -> bool {
    generating && message.role == MessageRole::User && message.id.starts_with("queued-user-")
}

pub fn current_turn_latest_tool(messages: &[ChatMessageInfo]) -> Option<&ToolActivityInfo> {
    messages
        .iter()
        .rev()
        .take_while(|message| {
            // Optimistic queue/steer echoes do not start a new accepted turn.
            message.role != MessageRole::User
                || message.id.starts_with("queued-user-")
                || message.id.starts_with("steered-user-")
        })
        .flat_map(|message| message.tool_activities.iter().rev())
        .next()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptRow {
    Message(usize),
    Activities(Range<usize>),
    Working,
}

pub fn is_activity_only(message: &ChatMessageInfo) -> bool {
    message.role == MessageRole::Assistant
        && message.content.is_empty()
        && message.reasoning_content.is_none()
        && message
            .tool_activities
            .iter()
            .any(|activity| activity.title != "update_plan")
}

pub fn build_transcript_rows(messages: &[ChatMessageInfo], generating: bool) -> Vec<TranscriptRow> {
    let mut rows = Vec::with_capacity(messages.len().saturating_add(1));
    let mut index = 0;
    while index < messages.len() {
        // Pending follow-ups live above the composer, not in the accepted transcript.
        if is_queued_message(&messages[index], generating) {
            index += 1;
            continue;
        }
        if !is_activity_only(&messages[index]) {
            rows.push(TranscriptRow::Message(index));
            index += 1;
            continue;
        }

        let start = index;
        while index < messages.len() && is_activity_only(&messages[index]) {
            index += 1;
        }
        rows.push(TranscriptRow::Activities(start..index));
    }
    if generating {
        rows.push(TranscriptRow::Working);
    }
    rows
}

pub fn grouped_tool_activities(
    messages: &[ChatMessageInfo],
) -> impl Iterator<Item = &ToolActivityInfo> + Clone {
    messages
        .iter()
        .flat_map(|message| message.tool_activities.iter())
        .filter(|activity| activity.title != "update_plan")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMatch {
    pub message_id: String,
    pub row_index: usize,
    pub excerpt: String,
}

/// Search the projected transcript, including unmounted rows, once per message.
pub fn find_conversation_messages(
    messages: &[ChatMessageInfo],
    generating: bool,
    query: &str,
) -> Vec<ConversationMatch> {
    let query = query.to_lowercase();
    if query.trim().is_empty() {
        return Vec::new();
    }
    build_transcript_rows(messages, generating)
        .into_iter()
        .enumerate()
        .filter_map(|(row_index, row)| {
            let TranscriptRow::Message(index) = row else {
                return None;
            };
            let message = &messages[index];
            if !matches!(message.role, MessageRole::User | MessageRole::Assistant)
                || message.content.is_empty()
                || !message.content.to_lowercase().contains(&query)
            {
                return None;
            }
            Some(ConversationMatch {
                message_id: message.id.clone(),
                row_index,
                excerpt: matching_excerpt(&message.content, &query),
            })
        })
        .collect()
}

fn matching_excerpt(content: &str, query: &str) -> String {
    // Lowercasing can expand a character (İ -> i + combining dot). Map the
    // folded byte offset back to source characters rather than slicing UTF-8.
    let folded = content.to_lowercase();
    let offset = folded.find(query).unwrap_or(0);
    let mut folded_bytes = 0;
    let match_char = content
        .chars()
        .take_while(|ch| {
            if folded_bytes >= offset {
                return false;
            }
            folded_bytes += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            true
        })
        .count();
    let start = match_char.saturating_sub(40);
    let mut chars = content.chars().skip(start);
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push('…');
    }
    // Keep source excerpts compact even for code with many blank lines.
    excerpt.extend(
        chars
            .by_ref()
            .take(160)
            .map(|ch| if ch.is_whitespace() { ' ' } else { ch }),
    );
    if chars.next().is_some() {
        excerpt.push('…');
    }
    excerpt
}

pub fn next_find_match(selected: Option<usize>, count: usize, previous: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match (selected, previous) {
        (Some(index), true) => (index + count - 1) % count,
        (Some(index), false) => (index + 1) % count,
        (None, true) => count - 1,
        (None, false) => 0,
    })
}

const PROMPT_EXCERPT_CHARS: usize = 96;

/// One user prompt in the transcript, in chronological order. Shared by the
/// conversation outline and composer prompt recall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptLandmark {
    pub message_id: String,
    /// 1-based index among user prompts ("Prompt N").
    pub ordinal: usize,
    /// Transcript row index at derivation time; revalidate before scrolling.
    pub row_index: usize,
    /// Full message content; composer recall loads this verbatim.
    pub text: String,
    /// Bounded, whitespace-normalized excerpt for compact lists. Empty when
    /// the message has no display text.
    pub excerpt: String,
    /// Optimistic queue/steer echo not yet confirmed by the session stream.
    pub pending_echo: bool,
}

/// Chronological user prompts derived from the projected transcript rows, so
/// activity-only and excluded messages never appear as landmarks.
pub fn prompt_landmarks(messages: &[ChatMessageInfo], generating: bool) -> Vec<PromptLandmark> {
    let mut landmarks = Vec::new();
    for (row_index, row) in build_transcript_rows(messages, generating)
        .iter()
        .enumerate()
    {
        let TranscriptRow::Message(index) = row else {
            continue;
        };
        let message = &messages[*index];
        if message.role != MessageRole::User {
            continue;
        }
        landmarks.push(PromptLandmark {
            message_id: message.id.clone(),
            ordinal: landmarks.len() + 1,
            row_index,
            text: message.content.clone(),
            excerpt: prompt_excerpt(&message.content),
            pending_echo: message.id.starts_with("queued-user-")
                || message.id.starts_with("steered-user-"),
        });
    }
    landmarks
}

/// Whitespace-normalized, `PROMPT_EXCERPT_CHARS`-bounded excerpt of a prompt
/// for compact list display; appends `…` when truncated.
fn prompt_excerpt(content: &str) -> String {
    let mut normalized = String::with_capacity(content.len());
    let mut last_was_space = true;
    for ch in content.trim().chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                normalized.push(' ');
            }
            last_was_space = true;
        } else {
            normalized.push(ch);
            last_was_space = false;
        }
    }
    let mut chars = normalized.chars();
    let excerpt: String = chars.by_ref().take(PROMPT_EXCERPT_CHARS).collect();
    if chars.next().is_some() {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

/// Outcome of stepping the composer prompt-recall cursor with Up/Down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptRecallStep {
    /// The key falls through to the default caret behavior.
    PassThrough,
    /// Load the entry at this index of the eligible prompt list.
    Load(usize),
    /// The cursor moved past the newest entry; restore the empty composer.
    Clear,
}

/// Older/newer stepping over eligible prompt entries. `position` is the
/// cursor's index into the entry list (`None` when the composer is not
/// browsing). Older navigation saturates at the oldest entry; newer
/// navigation past the newest entry clears the composer.
pub fn prompt_recall_step(position: Option<usize>, count: usize, older: bool) -> PromptRecallStep {
    if count == 0 {
        return PromptRecallStep::PassThrough;
    }
    match (position, older) {
        (None, true) => PromptRecallStep::Load(count - 1),
        (None, false) => PromptRecallStep::PassThrough,
        (Some(index), true) => PromptRecallStep::Load(index.saturating_sub(1)),
        (Some(index), false) if index + 1 >= count => PromptRecallStep::Clear,
        (Some(index), false) => PromptRecallStep::Load(index + 1),
    }
}

/// Arrow/Home/End stepping over an outline list; moves focus only, no wrap.
pub fn step_prompt_focus(focused: Option<usize>, count: usize, key: &str) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match key {
        "up" => Some(match focused {
            Some(index) if index > 0 => index - 1,
            _ => 0,
        }),
        "down" => Some(match focused {
            Some(index) => (index + 1).min(count - 1),
            None => 0,
        }),
        "home" => Some(0),
        "end" => Some(count - 1),
        _ => focused,
    }
}

use gpui::{FollowMode, ListAlignment, ListState, Window};
use std::sync::Arc;

/// Retained virtual transcript behavior shared by desktop and iOS.
/// Message rendering and native actions are supplied by the owning screen.
pub struct TranscriptState {
    pub list: ListState,
    pub messages: Arc<Vec<ChatMessageInfo>>,
    pub rows: Vec<TranscriptRow>,
    pub generating: bool,
}
impl TranscriptState {
    pub fn new(window: &Window) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, window.rem_size() * 37.5);
        list.set_follow_mode(FollowMode::Tail);
        Self {
            list,
            messages: Arc::new(Vec::new()),
            rows: Vec::new(),
            generating: false,
        }
    }
    pub fn sync(
        &mut self,
        messages: Arc<Vec<ChatMessageInfo>>,
        generating: bool,
        session_changed: bool,
        preserve_reading: bool,
    ) {
        if !session_changed
            && Arc::ptr_eq(&messages, &self.messages)
            && generating == self.generating
        {
            return;
        }

        let old_message_count = self.messages.len();
        let old_row_count = self.rows.len();
        let new_message_count = messages.len();

        if !session_changed
            && new_message_count == old_message_count
            && generating == self.generating
            // Hydration and streaming can change row topology without changing
            // message count (an activity-only reply gains visible content).
            && messages.iter().zip(self.messages.iter()).all(|(new, old)| {
                is_activity_only(new) == is_activity_only(old)
                    && is_queued_message(new, generating) == is_queued_message(old, generating)
            })
        {
            let last_changed =
                messages
                    .last()
                    .zip(self.messages.last())
                    .is_some_and(|(new, old)| {
                        new.id != old.id
                            || new.content.len() != old.content.len()
                            || new.reasoning_content.as_ref().map(String::len)
                                != old.reasoning_content.as_ref().map(String::len)
                            || new.tool_activities.len() != old.tool_activities.len()
                            || new.streaming != old.streaming
                    });
            self.messages = messages;
            if last_changed {
                self.list
                    .remeasure_items(old_row_count.saturating_sub(1)..old_row_count);
            } else {
                self.list.remeasure();
            }
            return;
        }

        let new_rows = build_transcript_rows(&messages, generating);
        let new_row_count = new_rows.len();
        // Only splice the Working row when the remaining rows are unchanged.
        // Generation toggles also filter queued messages, which requires a reset.
        let working_changed = !session_changed
            && new_message_count == old_message_count
            && generating != self.generating
            && new_rows
                .strip_suffix(&[TranscriptRow::Working])
                .unwrap_or(&new_rows)
                == self
                    .rows
                    .strip_suffix(&[TranscriptRow::Working])
                    .unwrap_or(&self.rows);
        let prepended = !session_changed
            && new_message_count > old_message_count
            && self
                .messages
                .first()
                .zip(messages.get(new_message_count - old_message_count))
                .is_some_and(|(old, new)| old.id == new.id)
            && self
                .messages
                .last()
                .zip(messages.last())
                .is_some_and(|(old, new)| old.id == new.id)
            && new_row_count >= old_row_count;
        let appended = !session_changed
            && new_message_count > old_message_count
            && self
                .messages
                .first()
                .zip(messages.first())
                .is_some_and(|(old, new)| old.id == new.id)
            && self
                .messages
                .last()
                .zip(messages.get(old_message_count.saturating_sub(1)))
                .is_some_and(|(old, new)| old.id == new.id)
            && new_row_count >= old_row_count;

        self.messages = messages;
        self.rows = new_rows;
        self.generating = generating;
        if working_changed && generating {
            self.list.splice(old_row_count..old_row_count, 1);
        } else if working_changed {
            self.list.splice(new_row_count..old_row_count, 0);
        } else if prepended {
            self.list.splice(0..0, new_row_count - old_row_count);
        } else if appended {
            self.list
                .splice(old_row_count..old_row_count, new_row_count - old_row_count);
        } else {
            // Reconciliation can replace every optimistic ID. Keep a find
            // reader's viewport, without guessing a new selected message.
            let reading_position =
                (preserve_reading && !session_changed && !self.list.is_following_tail())
                    .then(|| self.list.logical_scroll_top());
            self.list.reset(new_row_count);
            if let Some(mut position) = reading_position {
                position.item_ix = position.item_ix.min(new_row_count.saturating_sub(1));
                self.list.scroll_to(position);
            }
        }
        if session_changed {
            self.list.set_follow_mode(FollowMode::Tail);
        }
    }
}

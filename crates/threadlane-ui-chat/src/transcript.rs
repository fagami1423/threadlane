use std::ops::Range;

use threadlane_ui_state::{ChatMessageInfo, MessageRole, ToolActivityInfo};

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
    let query = query.trim().to_lowercase();
    if query.is_empty() {
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
                || !super::trajectory::contains_case_insensitive(&message.content, &query)
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

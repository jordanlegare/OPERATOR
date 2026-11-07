use std::collections::{HashMap, HashSet};

use crate::storage::{Entry, EntryKind};

#[derive(Debug, Clone)]
pub struct ConversationThread {
    /// Stable grouping key. New events use the Hermes session ID; legacy
    /// journals receive a deterministic fallback based on their first event.
    pub id: String,
    pub entries: Vec<usize>,
    pub first_entry: usize,
    pub turn_count: usize,
    pub tool_call_count: usize,
    pub title: String,
    pub streaming: bool,
}

pub fn is_hermes_event(kind: EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::HermesUser
            | EntryKind::HermesAssistant
            | EntryKind::HermesToolCall
            | EntryKind::HermesToolResult
            | EntryKind::HermesStreamStart
            | EntryKind::HermesStreamDelta
            | EntryKind::HermesStreamEnd
    )
}

/// Build conversation groups from immutable journal rows. Events captured by
/// current hooks share a session ID; older rows are grouped conservatively by
/// pairing each prompt with the following assistant and tool events.
pub fn build_threads(entries: &[Entry]) -> Vec<ConversationThread> {
    let mut threads: Vec<ConversationThread> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    let mut legacy_active: Option<String> = None;

    for (index, entry) in entries.iter().enumerate() {
        if !is_hermes_event(entry.kind) {
            continue;
        }

        let key = if let Some(session_id) = entry.conversation_id.as_deref().filter(|id| !id.is_empty()) {
            format!("session:{session_id}")
        } else if let Some(turn_id) = entry.turn_id.as_deref().filter(|id| !id.is_empty()) {
            format!("turn:{turn_id}")
        } else if entry.kind == EntryKind::HermesUser {
            let key = format!("legacy:{}", index + 1);
            legacy_active = Some(key.clone());
            key
        } else {
            legacy_active
                .clone()
                .unwrap_or_else(|| format!("legacy-orphan:{}", index + 1))
        };

        let position = *positions.entry(key.clone()).or_insert_with(|| {
            threads.push(ConversationThread {
                id: key.clone(),
                entries: Vec::new(),
                first_entry: index,
                turn_count: 0,
                tool_call_count: 0,
                title: String::new(),
                streaming: false,
            });
            threads.len() - 1
        });
        let thread = &mut threads[position];
        thread.entries.push(index);
        thread.first_entry = thread.first_entry.min(index);
        if entry.kind == EntryKind::HermesUser {
            thread.turn_count += 1;
            if thread.title.is_empty() {
                thread.title = preview(&entry.text, 96);
            }
        }
        if entry.kind == EntryKind::HermesToolCall {
            thread.tool_call_count += 1;
        }
        if thread.title.is_empty() {
            thread.title = preview(&entry.text, 96);
        }
    }

    for thread in &mut threads {
        let mut active_streams: HashSet<(String, u32)> = HashSet::new();
        let mut turns = HashSet::new();
        let mut calls = HashSet::new();
        for &index in &thread.entries {
            let entry = &entries[index];
            if let Some(turn_id) = entry.turn_id.as_ref() {
                turns.insert(turn_id.clone());
            }
            if let Some(call_id) = entry.tool_call_id.as_ref() {
                calls.insert(call_id.clone());
            }
            let stream_key = (
                entry.turn_id.clone().unwrap_or_default(),
                entry.iteration.unwrap_or_default(),
            );
            match entry.kind {
                EntryKind::HermesStreamStart | EntryKind::HermesStreamDelta => {
                    active_streams.insert(stream_key);
                }
                EntryKind::HermesStreamEnd => {
                    active_streams.remove(&stream_key);
                }
                EntryKind::HermesAssistant => {
                    if let Some(turn_id) = entry.turn_id.as_ref() {
                        active_streams.retain(|(active_turn, _)| active_turn != turn_id);
                    }
                }
                _ => {}
            }
        }
        if !turns.is_empty() {
            thread.turn_count = turns.len();
        }
        if !calls.is_empty() {
            thread.tool_call_count = calls.len();
        }
        thread.streaming = !active_streams.is_empty();
        if thread.title.is_empty() {
            thread.title = "(conversation without a text preview)".to_string();
        }
    }

    threads.sort_by_key(|thread| thread.first_entry);
    threads
}

pub fn thread_for_entry(entries: &[Entry], entry_index: usize) -> Option<String> {
    build_threads(entries)
        .into_iter()
        .find(|thread| thread.entries.contains(&entry_index))
        .map(|thread| thread.id)
}

fn preview(text: &str, limit: usize) -> String {
    let first_line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    let flattened = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flattened.chars();
    let preview = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::{build_threads, thread_for_entry};
    use crate::storage::{Entry, EntryKind};

    fn event(kind: EntryKind, text: &str, session: Option<&str>, turn: Option<&str>) -> Entry {
        let mut entry = Entry::with_kind(kind, text.to_string());
        entry.conversation_id = session.map(str::to_string);
        entry.turn_id = turn.map(str::to_string);
        entry
    }

    #[test]
    fn groups_prompts_answers_and_tools_by_session_across_other_journal_rows() {
        let entries = vec![
            event(EntryKind::HermesUser, "first prompt", Some("session-a"), Some("turn-1")),
            Entry::with_kind(EntryKind::Note, "unrelated note".to_string()),
            event(EntryKind::HermesToolCall, "call", Some("session-a"), Some("turn-1")),
            event(EntryKind::HermesToolResult, "result", Some("session-a"), Some("turn-1")),
            event(EntryKind::HermesAssistant, "answer", Some("session-a"), Some("turn-1")),
            event(EntryKind::HermesUser, "second prompt", Some("session-a"), Some("turn-2")),
            event(EntryKind::HermesAssistant, "second answer", Some("session-a"), Some("turn-2")),
            event(EntryKind::HermesUser, "other session", Some("session-b"), Some("turn-x")),
        ];

        let threads = build_threads(&entries);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].id, "session:session-a");
        assert_eq!(threads[0].entries, vec![0, 2, 3, 4, 5, 6]);
        assert_eq!(threads[0].turn_count, 2);
        assert_eq!(threads[0].tool_call_count, 1);
        assert_eq!(threads[1].id, "session:session-b");
        assert_eq!(thread_for_entry(&entries, 3).as_deref(), Some("session:session-a"));
    }

    #[test]
    fn legacy_rows_pair_each_prompt_with_following_assistant_and_tool_events() {
        let entries = vec![
            event(EntryKind::HermesUser, "prompt one", None, None),
            event(EntryKind::HermesToolCall, "call", None, None),
            event(EntryKind::HermesToolResult, "result", None, None),
            event(EntryKind::HermesAssistant, "answer one", None, None),
            event(EntryKind::HermesUser, "prompt two", None, None),
            event(EntryKind::HermesAssistant, "answer two", None, None),
        ];
        let threads = build_threads(&entries);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].entries, vec![0, 1, 2, 3]);
        assert_eq!(threads[1].entries, vec![4, 5]);
    }

    #[test]
    fn stream_end_closes_the_live_indicator_for_its_turn_and_iteration() {
        let mut start = event(EntryKind::HermesStreamStart, "start", Some("session"), Some("turn"));
        start.iteration = Some(2);
        let mut delta = event(EntryKind::HermesStreamDelta, "hello", Some("session"), Some("turn"));
        delta.iteration = Some(2);
        let mut end = event(EntryKind::HermesStreamEnd, "finished", Some("session"), Some("turn"));
        end.iteration = Some(2);
        let entries = vec![start, delta, end];
        assert!(!build_threads(&entries)[0].streaming);
    }
}

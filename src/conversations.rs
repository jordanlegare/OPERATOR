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
            | EntryKind::HermesHistory
            | EntryKind::HermesInterim
            | EntryKind::HermesSession
    )
}

/// Build conversation groups from immutable journal rows. Events captured by
/// current hooks share a session ID; older rows are grouped conservatively by
/// pairing each prompt with the following assistant and tool events.
pub fn build_threads(entries: &[Entry]) -> Vec<ConversationThread> {
    let mut threads: Vec<ConversationThread> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    // Hooks can omit session_id or turn_id on individual event types. Keep
    // those events attached to the most recently active conversation, and
    // remember turn-to-thread associations when a turn-only event arrives.
    let mut active_thread: Option<String> = None;
    let mut turn_threads: HashMap<String, String> = HashMap::new();

    for (index, entry) in entries.iter().enumerate() {
        if !is_hermes_event(entry.kind) {
            continue;
        }

        let session_id = entry.conversation_id.as_deref().filter(|id| !id.is_empty());
        let turn_id = entry.turn_id.as_deref().filter(|id| !id.is_empty());

        let key = if let Some(session_id) = session_id {
            let key = format!("session:{session_id}");
            if let Some(turn_id) = turn_id {
                turn_threads.insert(turn_id.to_string(), key.clone());
            }
            active_thread = Some(key.clone());
            key
        } else if entry.kind == EntryKind::HermesUser {
            let key = turn_id
                .map(|turn_id| format!("turn:{turn_id}"))
                .unwrap_or_else(|| format!("legacy:{}", index + 1));
            if let Some(turn_id) = turn_id {
                turn_threads.insert(turn_id.to_string(), key.clone());
            }
            active_thread = Some(key.clone());
            key
        } else if let Some(turn_id) = turn_id {
            if let Some(key) = turn_threads.get(turn_id).cloned() {
                active_thread = Some(key.clone());
                key
            } else if let Some(key) = active_thread.clone() {
                turn_threads.insert(turn_id.to_string(), key.clone());
                key
            } else {
                let key = format!("turn:{turn_id}");
                turn_threads.insert(turn_id.to_string(), key.clone());
                active_thread = Some(key.clone());
                key
            }
        } else {
            active_thread
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
        // Prefer an actual user prompt as the thread title; context snapshots
        // and lifecycle markers are metadata, not the conversation's subject.
        if entry.kind == EntryKind::HermesUser {
            thread.turn_count += 1;
            if thread.title.is_empty() {
                thread.title = preview(&entry.text, 96);
            }
        }
        if entry.kind == EntryKind::HermesToolCall {
            thread.tool_call_count += 1;
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
                    active_streams.remove(&stream_key);
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


/// Collapse every streamed turn into one assistant block while it is in progress.
/// Tool calls do not suppress streaming. Once an assistant event arrives after the
/// latest stream delta and latest tool event, it is treated as the authoritative
/// final answer and replaces the accumulated live block.
#[derive(Debug, Default)]
pub struct CollapsedLiveStreams {
    /// Combined text keyed by the first journal index for each in-progress turn.
    pub text_by_first_index: HashMap<usize, String>,
    /// Delta entries folded into the first live block, or hidden once finalized.
    pub hidden_delta_indices: HashSet<usize>,
    /// Intermediate assistant events replaced by the live block or final answer.
    pub replaced_assistant_indices: HashSet<usize>,
}

pub fn collapse_live_streams(entries: &[(usize, &Entry)]) -> CollapsedLiveStreams {
    let mut current_turn = None::<String>;
    let mut streams = HashMap::<String, (usize, String, Vec<usize>, usize)>::new();
    let mut assistant_entries = HashMap::<String, Vec<usize>>::new();
    let mut last_tool_indices = HashMap::<String, usize>::new();

    for (index, entry) in entries {
        if entry.kind == EntryKind::HermesUser {
            current_turn = Some(
                entry
                    .turn_id
                    .clone()
                    .unwrap_or_else(|| format!("implicit-turn-{index}")),
            );
        } else if let Some(turn_id) = entry.turn_id.as_ref() {
            current_turn = Some(turn_id.clone());
        }

        let turn_key = current_turn
            .clone()
            .unwrap_or_else(|| "orphan-turn".to_string());

        match entry.kind {
            EntryKind::HermesStreamDelta => {
                let stream = streams
                    .entry(turn_key)
                    .or_insert_with(|| (*index, String::new(), Vec::new(), *index));
                stream.1.push_str(&entry.text);
                stream.2.push(*index);
                stream.3 = *index;
            }
            EntryKind::HermesAssistant => {
                assistant_entries
                    .entry(turn_key)
                    .or_default()
                    .push(*index);
            }
            EntryKind::HermesToolCall | EntryKind::HermesToolResult => {
                last_tool_indices.insert(turn_key, *index);
            }
            _ => {}
        }
    }

    let mut collapsed = CollapsedLiveStreams::default();

    for (turn, (first_index, text, delta_indices, last_delta_index)) in streams {
        // An assistant event only finalizes this stream if it follows both
        // the latest streamed text and the latest tool activity. An assistant
        // payload emitted before a subsequent tool call is still intermediate.
        let last_tool_index = last_tool_indices.get(&turn).copied();
        let final_assistant_index = assistant_entries.get(&turn).and_then(|indices| {
            indices
                .iter()
                .rev()
                .find(|assistant_index| {
                    **assistant_index > last_delta_index
                        && last_tool_index
                            .map(|tool_index| **assistant_index > tool_index)
                            .unwrap_or(true)
                })
                .copied()
        });

        if final_assistant_index.is_some() {
            // The authoritative final assistant event is rendered in its own
            // journal position; hide all delta rows to prevent duplicate text.
            collapsed.hidden_delta_indices.extend(delta_indices);
        } else {
            // While the turn is active, keep the accumulated text visible at
            // its first delta row and fold subsequent chunks into that block.
            collapsed.text_by_first_index.insert(first_index, text);
            collapsed.hidden_delta_indices.extend(
                delta_indices
                    .into_iter()
                    .filter(|index| *index != first_index),
            );
        }

        if let Some(indices) = assistant_entries.get(&turn) {
            collapsed.replaced_assistant_indices.extend(
                indices
                    .iter()
                    .copied()
                    .filter(|index| Some(*index) != final_assistant_index),
            );
        }
    }

    collapsed
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
    fn initial_history_snapshot_and_lifecycle_events_join_the_same_session_thread() {
        let history = event(
            EntryKind::HermesHistory,
            "PRIOR CONVERSATION CONTEXT\\n\\n[1] USER\\nEarlier question",
            Some("session-resumed"),
            None,
        );
        let started = event(
            EntryKind::HermesSession,
            "Hermes session started.",
            Some("session-resumed"),
            None,
        );
        let interim = event(
            EntryKind::HermesInterim,
            "I am checking the available context.",
            Some("session-resumed"),
            Some("turn-current"),
        );
        let prompt = event(
            EntryKind::HermesUser,
            "Continue from there.",
            Some("session-resumed"),
            Some("turn-current"),
        );
        let assistant = event(
            EntryKind::HermesAssistant,
            "Completed answer.",
            Some("session-resumed"),
            Some("turn-current"),
        );

        let threads = build_threads(&[history, started, interim, prompt, assistant]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "session:session-resumed");
        assert_eq!(threads[0].entries, vec![0, 1, 2, 3, 4]);
        assert_eq!(threads[0].turn_count, 1);
        assert!(threads[0].title.contains("Continue from there."));
    }

    #[test]
    fn turn_only_tool_and_assistant_events_stay_with_session_prompt() {
        let prompt = event(EntryKind::HermesUser, "check the headlines", Some("session-a"), Some("turn-1"));
        let tool_call = event(EntryKind::HermesToolCall, "search", None, Some("turn-1"));
        let tool_result = event(EntryKind::HermesToolResult, "results", None, Some("turn-1"));
        let assistant = event(EntryKind::HermesAssistant, "Here is the final answer.", None, Some("turn-1"));

        let threads = build_threads(&[prompt, tool_call, tool_result, assistant]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "session:session-a");
        assert_eq!(threads[0].entries, vec![0, 1, 2, 3]);
        assert!(threads[0].title.contains("check the headlines"));
    }

    #[test]
    fn events_without_identifiers_inherit_the_active_session_thread() {
        let prompt = event(EntryKind::HermesUser, "check the headlines", Some("session-a"), Some("turn-1"));
        let tool_call = event(EntryKind::HermesToolCall, "search", None, None);
        let assistant = event(EntryKind::HermesAssistant, "Here is the final answer.", None, None);

        let threads = build_threads(&[prompt, tool_call, assistant]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "session:session-a");
        assert_eq!(threads[0].entries, vec![0, 1, 2]);
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
    fn completing_one_iteration_does_not_close_another_iteration_in_the_same_turn() {
        let mut started = event(EntryKind::HermesStreamStart, "start", Some("session"), Some("turn"));
        started.iteration = Some(0);
        let mut ended = event(EntryKind::HermesStreamEnd, "finished", Some("session"), Some("turn"));
        ended.iteration = Some(1);
        let threads = build_threads(&[started, ended]);
        assert!(threads[0].streaming);
    }

    #[test]
    fn stream_deltas_across_iterations_remain_live_until_final_assistant_arrives() {
        use super::collapse_live_streams;

        let prompt = event(EntryKind::HermesUser, "prompt", Some("session"), Some("turn-1"));
        let mut first = event(EntryKind::HermesStreamDelta, "The basic ", Some("session"), Some("turn-1"));
        first.iteration = Some(0);
        let end = event(EntryKind::HermesStreamEnd, "finished", Some("session"), Some("turn-1"));
        let tool_call = event(EntryKind::HermesToolCall, "Tool: lookup", Some("session"), Some("turn-1"));
        let tool_result = event(EntryKind::HermesToolResult, "lookup complete", Some("session"), Some("turn-1"));
        let mut second = event(EntryKind::HermesStreamDelta, "necessities of life.", Some("session"), Some("turn-1"));
        second.iteration = Some(1);
        let entries = vec![
            (0, &prompt),
            (1, &first),
            (2, &end),
            (3, &tool_call),
            (4, &tool_result),
            (5, &second),
        ];

        let collapsed = collapse_live_streams(&entries);
        assert_eq!(
            collapsed.text_by_first_index.get(&1).map(String::as_str),
            Some("The basic necessities of life.")
        );
        assert!(collapsed.hidden_delta_indices.contains(&5));
        assert!(collapsed.replaced_assistant_indices.is_empty());
    }

    #[test]
    fn final_assistant_replaces_live_stream_after_tool_activity() {
        use super::collapse_live_streams;

        let prompt = event(EntryKind::HermesUser, "run a tool", Some("session"), Some("turn-1"));
        let first_delta = event(EntryKind::HermesStreamDelta, "I will inspect ", Some("session"), Some("turn-1"));
        let intermediate_assistant = event(EntryKind::HermesAssistant, "I will inspect the system.", Some("session"), Some("turn-1"));
        let tool_call = event(EntryKind::HermesToolCall, "Tool: terminal", Some("session"), Some("turn-1"));
        let tool_result = event(EntryKind::HermesToolResult, "command output", Some("session"), Some("turn-1"));
        let second_delta = event(EntryKind::HermesStreamDelta, "The result is ready.", Some("session"), Some("turn-1"));
        let final_assistant = event(EntryKind::HermesAssistant, "Here is the completed answer.", Some("session"), Some("turn-1"));
        let entries = vec![
            (0, &prompt),
            (1, &first_delta),
            (2, &intermediate_assistant),
            (3, &tool_call),
            (4, &tool_result),
            (5, &second_delta),
            (6, &final_assistant),
        ];

        let collapsed = collapse_live_streams(&entries);
        assert!(collapsed.text_by_first_index.is_empty());
        assert_eq!(collapsed.hidden_delta_indices, [1, 5].into_iter().collect());
        assert_eq!(collapsed.replaced_assistant_indices, [2].into_iter().collect());
        assert!(!collapsed.replaced_assistant_indices.contains(&6));
    }

    #[test]
    fn legacy_tool_using_turn_keeps_stream_live_until_final_assistant() {
        use super::collapse_live_streams;

        let prompt = event(EntryKind::HermesUser, "run a tool", None, None);
        let delta = event(EntryKind::HermesStreamDelta, "intermediate output", None, None);
        let tool_call = event(EntryKind::HermesToolCall, "Tool: terminal", None, None);
        let tool_result = event(EntryKind::HermesToolResult, "tool finished", None, None);
        let entries = vec![(10, &prompt), (11, &delta), (12, &tool_call), (13, &tool_result)];

        let collapsed = collapse_live_streams(&entries);
        assert_eq!(
            collapsed.text_by_first_index.get(&11).map(String::as_str),
            Some("intermediate output")
        );
        assert!(collapsed.hidden_delta_indices.is_empty());
        assert!(collapsed.replaced_assistant_indices.is_empty());
    }

    #[test]
    fn legacy_tool_using_turn_final_assistant_replaces_stream() {
        use super::collapse_live_streams;

        let prompt = event(EntryKind::HermesUser, "run a tool", None, None);
        let delta = event(EntryKind::HermesStreamDelta, "intermediate output", None, None);
        let tool_call = event(EntryKind::HermesToolCall, "Tool: terminal", None, None);
        let assistant = event(EntryKind::HermesAssistant, "final answer", None, None);
        let entries = vec![(10, &prompt), (11, &delta), (12, &tool_call), (13, &assistant)];

        let collapsed = collapse_live_streams(&entries);
        assert!(collapsed.text_by_first_index.is_empty());
        assert_eq!(collapsed.hidden_delta_indices, [11].into_iter().collect());
        assert!(collapsed.replaced_assistant_indices.is_empty());
    }

    #[test]
    fn legacy_stream_deltas_without_turn_ids_are_replaced_by_final_assistant() {
        use super::collapse_live_streams;

        let prompt = event(EntryKind::HermesUser, "prompt", None, None);
        let mut first = event(EntryKind::HermesStreamDelta, "hello ", None, None);
        first.iteration = Some(0);
        let mut second = event(EntryKind::HermesStreamDelta, "world", None, None);
        second.iteration = Some(1);
        let assistant = event(EntryKind::HermesAssistant, "final answer", None, None);
        let entries = vec![(10, &prompt), (11, &first), (12, &second), (13, &assistant)];

        let collapsed = collapse_live_streams(&entries);
        assert!(collapsed.text_by_first_index.is_empty());
        assert_eq!(
            collapsed.hidden_delta_indices,
            [11, 12].into_iter().collect()
        );
        assert!(collapsed.replaced_assistant_indices.is_empty());
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

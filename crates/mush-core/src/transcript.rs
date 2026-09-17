//! The transcript algebra: the rules that decide the *shape* of a request.
//!
//! Pairing tool calls with their results, repairing arguments a model sent as
//! something other than JSON, dropping the oldest turns to fit a budget, and
//! deciding when a conversation should be folded into a summary. Nothing here
//! calls a model or touches an actor — these are pure functions over
//! `Message`s, which is why they live in core and not in the run loop that
//! applies them.

use serde_json::Value;

use crate::message::Message;

/// Ceiling on a compaction summary. A summary is prose, not a transcript, but
/// reasoning tokens count against it too.
pub const COMPACT_REPLY_TOKENS: u32 = 10_240;

/// The instruction appended when the transcript nears the context window.
pub const COMPACT_INSTRUCTION: &str = "\
The conversation is approaching the context limit. Summarize everything \
important so far — the original task, the work done, files created or \
changed, open issues, and the current state. This summary replaces the \
conversation, so include every fact the task still depends on. Reply with \
just the summary.";

/// Approaching the context window: fold the conversation into a summary
/// instead of dropping old turns, so long-running tasks keep their state. The
/// summarize request re-sends the history, so only fire while it still fits;
/// beyond that, trimming stays the last resort.
pub fn needs_compaction(messages: &[Message], budget: usize) -> bool {
    let history: usize = messages.iter().map(Message::weight).sum();
    history > budget * 3 / 4 && history <= budget
}

/// A transcript adopted from the UI can interleave the human's steering with a
/// tool batch (they typed while the tools ran) or hold calls whose run was
/// interrupted before it recorded a result. Strict servers reject both shapes,
/// so pull every batch's results back beside the assistant message that asked
/// for them, then answer whatever is still missing.
pub fn repair_tool_pairs(messages: &mut Vec<Message>) {
    let mut index = 0;
    while index < messages.len() {
        // Normalize before pairing: a transcript adopted from an older session
        // may hold a call with no id, and a result has to answer an id that
        // exists. Idempotent, so a valid transcript is untouched.
        messages[index].ensure_tool_call_ids();
        let calls: Vec<String> = messages[index]
            .tool_calls()
            .iter()
            .map(|call| call.id.clone())
            .collect();
        if calls.is_empty() {
            index += 1;
            continue;
        }
        // Results belong immediately after their assistant message. Anything
        // else that sat in between (a nudge, usually) keeps its order after the
        // batch — which is where the actor folded it at runtime.
        let mut cursor = index + 1;
        let mut insert_at = index + 1;
        while cursor < messages.len() && messages[cursor].role != "assistant" {
            let answers_a_call = messages[cursor].role == "tool"
                && messages[cursor]
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| calls.iter().any(|call| call == id));
            if answers_a_call {
                if cursor != insert_at {
                    let result = messages.remove(cursor);
                    messages.insert(insert_at, result);
                }
                cursor += 1;
                insert_at += 1;
            } else {
                cursor += 1;
            }
        }
        // A call with no result (the run was interrupted mid-batch) would
        // dangle forever; give it an explicit error the model can act on.
        for id in &calls {
            let answered = messages[index + 1..insert_at]
                .iter()
                .any(|message| message.tool_call_id.as_deref() == Some(id.as_str()));
            if !answered {
                messages.insert(
                    insert_at,
                    Message::tool(
                        id.clone(),
                        "error: no result was recorded for this call (the run was interrupted)",
                    ),
                );
                insert_at += 1;
            }
        }
        index = insert_at;
    }
}

/// A model occasionally emits `tool_call` arguments that are not valid JSON.
/// Sending that message back into history verbatim makes some servers reject
/// the whole request with a parse error; rewrite invalid arguments to `{}` so
/// the tool executor returns a clear per-call error instead.
///
/// The same pass gives every call an id a strict server accepts, because the
/// id is what pairs a call with its result: a batch that arrives with a missing
/// or repeated id would otherwise be answered with `tool_call_id: ""` (or the
/// duplicate) and be rejected.
pub fn sanitize_tool_calls(mut message: Message) -> Message {
    message.ensure_tool_call_ids();
    let Some(calls) = message.tool_calls.as_mut() else {
        return message;
    };
    for call in calls {
        // Arguments must be a JSON object; a bare string passes JSON parsing
        // but makes servers reject the message outright.
        if !matches!(
            serde_json::from_str::<Value>(&call.function.arguments),
            Ok(Value::Object(_))
        ) {
            call.function.arguments = "{}".to_string();
        }
    }
    message
}

/// Drop the oldest turns until the conversation fits the budget. Trimming at a
/// user message keeps assistant/tool pairs intact, which servers validate.
/// The budget comes from the endpoint's context window.
pub fn trim_history(messages: &mut Vec<Message>, budget: usize) {
    loop {
        let total: usize = messages.iter().map(Message::weight).sum();
        if total <= budget {
            return;
        }
        let user_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "user")
            .map(|(i, _)| i)
            .collect();
        // `messages[0]` is the system prompt and `messages[1]` the opening
        // task — for subagents that is the parent's brief, which must survive
        // trimming. System + task + the newest turn is the minimum shape.
        if user_indices.len() < 3 {
            return;
        }
        // Drop the oldest full turn: everything after the task message up to
        // the third user message, cutting at user boundaries so pairs stay
        // valid. Guarded so the drain can never be a no-op (which would spin
        // here forever) on a transcript that does not start with system+user.
        let keep_from = user_indices[2];
        if keep_from <= 2 {
            return;
        }
        messages.drain(2..keep_from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionCall, ToolCall};

    #[test]
    fn trim_history_keeps_recent_turns_and_pairs() {
        let mut messages = vec![Message::system("you are mush"), Message::user("first")];
        for i in 0..200 {
            messages.push(Message::assistant(format!("reply {i} {}", "x".repeat(500))));
            messages.push(Message::tool(format!("call{i}"), "result"));
            messages.push(Message::user(format!("again {i}")));
        }
        // Mirror the 8K-context default budget from Config::history_budget.
        trim_history(&mut messages, 15_000);
        assert_eq!(messages[0].role, "system");
        assert!(messages.iter().map(Message::weight).sum::<usize>() <= 15_000);
        // The first kept entry must be a user message so pairs stay valid.
        assert_eq!(messages[1].role, "user");
    }

    /// A transcript that does not open with `system, user` must not make the
    /// trimmer spin: the guard returns instead of draining nothing forever.
    #[test]
    fn trim_history_terminates_on_a_system_less_transcript() {
        let mut messages = vec![Message::user("a"), Message::user("b"), Message::user("c")];
        trim_history(&mut messages, 0);
        assert_eq!(
            messages.len(),
            3,
            "nothing can be trimmed without a pair to keep"
        );
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        }
    }

    fn assistant_calling(ids: &[&str]) -> Message {
        let mut message = Message::assistant("working");
        message.tool_calls = Some(ids.iter().map(|id| call(id)).collect());
        message
    }

    fn roles(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.role.as_str())
            .collect()
    }

    /// The human typed while a tool batch was running: the UI's transcript puts
    /// their words between the assistant's calls and the results. Adopting it
    /// verbatim would make strict servers reject every later request.
    #[test]
    fn steering_inside_a_tool_batch_moves_after_the_results() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a", "b"]),
            Message::user("actually, also do X"),
            Message::tool("a", "result a"),
            Message::tool("b", "result b"),
            Message::assistant("done"),
        ];
        repair_tool_pairs(&mut messages);

        assert_eq!(
            roles(&messages),
            [
                "system",
                "user",
                "assistant",
                "tool",
                "tool",
                "user",
                "assistant"
            ]
        );
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("a"));
        assert_eq!(messages[4].tool_call_id.as_deref(), Some("b"));
        assert_eq!(messages[5].text(), "actually, also do X");
    }

    /// Quitting during a batch can persist an assistant message whose tool
    /// calls never got results; the next request must still be answerable.
    #[test]
    fn a_dangling_tool_call_is_answered_with_an_error() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a", "b"]),
            Message::user("carry on"),
        ];
        repair_tool_pairs(&mut messages);

        assert_eq!(
            roles(&messages),
            ["system", "user", "assistant", "tool", "tool", "user"]
        );
        assert!(messages[3].text().starts_with("error:"));
        assert!(messages[4].text().starts_with("error:"));
    }

    /// An already-valid transcript must come out byte-for-byte unchanged.
    #[test]
    fn repair_leaves_a_valid_transcript_alone() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::tool("a", "result"),
            Message::user("next"),
        ];
        let before = serde_json::to_string(&messages).unwrap();
        repair_tool_pairs(&mut messages);
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);
    }

    /// Every reply the run loop sanitizes leaves with a non-empty, unique id
    /// per call — the ids its results will answer.
    #[test]
    fn sanitize_gives_every_call_an_id_to_answer() {
        let mut message = Message::assistant("working");
        message.tool_calls = Some(vec![call(""), call("dup"), call("dup")]);
        let repaired = sanitize_tool_calls(message);
        let ids: Vec<&str> = repaired
            .tool_calls()
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert!(!ids.iter().any(|id| id.is_empty()), "{ids:?}");
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        // The message that goes back into history carries the same ids the run
        // answers, so the pairing the server checks is exact.
        let sent = serde_json::to_string(&repaired).unwrap();
        for id in &ids {
            assert!(sent.contains(&format!("\"id\":\"{id}\"")), "{sent}");
        }
    }

    /// An adopted transcript whose call has no id is repaired the same way, so
    /// the error result inserted for a dangling call answers a real id.
    #[test]
    fn a_dangling_call_with_no_id_is_answered_with_one() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&[""]),
        ];
        repair_tool_pairs(&mut messages);

        assert_eq!(roles(&messages), ["system", "user", "assistant", "tool"]);
        let id = messages[2].tool_calls()[0].id.clone();
        assert!(!id.is_empty(), "the call was given an id");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some(id.as_str()));
        assert!(messages[3].text().starts_with("error:"));
    }

    #[test]
    fn sanitize_repairs_invalid_tool_call_json() {
        let mut message = Message::assistant("here you go");
        message.tool_calls = Some(vec![
            ToolCall {
                id: "a".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: "{\"path\": \"ok.rs\"}".into(),
                },
            },
            ToolCall {
                id: "b".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "edit_file".into(),
                    arguments: "\"Please retry with a smaller context\"".into(),
                },
            },
        ]);
        let repaired = sanitize_tool_calls(message);
        let calls = repaired.tool_calls();
        assert_eq!(calls[0].function.arguments, "{\"path\": \"ok.rs\"}");
        assert_eq!(calls[1].function.arguments, "{}");
    }
}

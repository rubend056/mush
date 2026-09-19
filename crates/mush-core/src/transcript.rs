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
///
/// The last sentence is where "do not call a tool" lives, and it lives *here* on
/// purpose. The request this travels with is the run's own request plus this one
/// user message and nothing else — same system prompt, same tools, same
/// `tool_choice` — because the tools are the head of the prompt and the endpoint
/// caches prefixes: a summarize call that drops them is a cache miss over an
/// entire history, at the moment that history is at its largest. Asking for
/// prose in the message costs nothing and is something the model can act on,
/// where a `tool_choice` in the request body is not part of what it is asked.
pub const COMPACT_INSTRUCTION: &str = "\
The conversation is approaching the context limit. Summarize everything \
important so far — the original task, the work done, files created or \
changed, open issues, and the current state. This summary replaces the \
conversation, so include every fact the task still depends on. Reply with \
just the summary, as plain text, and end your turn: call no tool.";

/// The history size at which compaction fires: three quarters of the budget,
/// in bytes.
///
/// `budget_bytes` is the same unit [`Message::weight`] counts and the same unit
/// [`Config::history_budget`](crate::config::Config::history_budget) returns —
/// the bytes-per-token conversion lives there, once, and is saturating too. The
/// multiply saturates rather than wrapping, because nothing downstream can
/// tell a budget that was never converted from one that was: `usize::MAX` from
/// a caller that forgot the conversion (or a window a hostile endpoint
/// advertised) would otherwise wrap `* 3` down to nearly nothing and mark a
/// two-message transcript as needing a fold.
pub fn compaction_trigger(budget_bytes: usize) -> usize {
    budget_bytes.saturating_mul(3) / 4
}

/// Approaching the context window: fold the conversation into a summary
/// instead of dropping old turns, so long-running tasks keep their state. The
/// summarize request re-sends the history, so only fire while it still fits;
/// beyond that, trimming stays the last resort.
///
/// `budget_bytes` is in bytes, the unit [`Message::weight`] weighs a transcript
/// in. The lower bound is strict — a transcript *at* the trigger is not yet
/// worth folding — and the upper one is inclusive: a transcript at exactly the
/// whole budget is still one the summarize request can carry, and anything past
/// it belongs to [`trim_history`], which drops turns instead of asking a model
/// to read history the endpoint would reject.
pub fn needs_compaction(messages: &[Message], budget_bytes: usize) -> bool {
    let history: usize = messages.iter().map(Message::weight).sum();
    history > compaction_trigger(budget_bytes) && history <= budget_bytes
}

/// A transcript adopted from the UI can interleave the human's steering with a
/// tool batch (they typed while the tools ran) or hold calls whose run was
/// interrupted before it recorded a result. Strict servers reject both shapes,
/// so pull every batch's results back beside the assistant message that asked
/// for them, then answer whatever is still missing.
pub fn repair_tool_pairs(messages: &mut Vec<Message>) {
    let mut index = 0;
    while index < messages.len() {
        // Ids are not normalized here: every message parsed from the wire or
        // from `session.json` already got them in `Message`'s own deserializer
        // (`tool_calls_from_wire`), and nothing else builds a message with
        // tool calls.
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
/// The ids are already the deserializer's (`tool_calls_from_wire`): a batch
/// with a missing or repeated id got one there, so it cannot arrive here as
/// `tool_call_id: ""` for the result to answer.
pub fn sanitize_tool_calls(mut message: Message) -> Message {
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

    /// A one-message transcript weighing exactly `weight` bytes. `Message::weight`
    /// counts the role plus the text, so the text is sized to land on the
    /// number instead of being padded until the assertion happens to hold.
    fn transcript_of_weight(weight: usize) -> Vec<Message> {
        vec![Message::user("x".repeat(weight - "user".len()))]
    }

    /// The trigger is computed, not written a second time: three quarters of
    /// the budget, saturating.
    #[test]
    fn the_trigger_is_three_quarters_of_the_budget() {
        assert_eq!(compaction_trigger(0), 0);
        assert_eq!(compaction_trigger(1_000), 750);
        assert_eq!(compaction_trigger(7_501), 5_625);
    }

    /// A budget that is not bytes at all — `usize::MAX`, what a caller that
    /// skipped the bytes-per-token conversion in `Config::history_budget`
    /// hands over — must neither panic nor lie. `budget * 3 / 4` panicked here
    /// in a debug build, and in a release one it wrapped: at
    /// `6_148_914_691_236_517_206` three times the budget wraps to 2, so the
    /// old expression returned 0 and marked *every* transcript, however small,
    /// as needing a fold. Saturating, the trigger stays a quarter of the
    /// budget and an ordinary transcript is nowhere near it.
    #[test]
    fn a_nonsense_budget_does_not_wrap_the_trigger_to_zero() {
        assert_eq!(compaction_trigger(usize::MAX), usize::MAX / 4);
        assert_eq!(
            compaction_trigger(6_148_914_691_236_517_206),
            usize::MAX / 4
        );
        let messages = vec![Message::system("you are mush"), Message::user("task")];
        assert!(!needs_compaction(&messages, usize::MAX));
        assert!(!needs_compaction(&messages, 6_148_914_691_236_517_206));
    }

    /// The fold fires strictly past the trigger: one byte over folds, and the
    /// trigger itself — the boundary the comparison has always had — does not,
    /// so a transcript parked exactly on it is not re-summarized forever.
    #[test]
    fn compaction_fires_one_byte_past_the_trigger_and_not_at_it() {
        let budget = 1_000;
        let trigger = compaction_trigger(budget);
        assert!(!needs_compaction(&transcript_of_weight(trigger), budget));
        assert!(needs_compaction(&transcript_of_weight(trigger + 1), budget));
    }

    /// Past the whole budget the fold must not fire: the summarize request
    /// re-sends the history, and one the endpoint will reject is not a
    /// summary, it is a failed request. Trimming is what handles that range.
    #[test]
    fn a_transcript_past_the_whole_budget_does_not_fold() {
        let budget = 1_000;
        assert!(needs_compaction(
            &transcript_of_weight(compaction_trigger(budget) + 1),
            budget
        ));
        assert!(!needs_compaction(&transcript_of_weight(budget + 1), budget));
    }

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

    /// An adopted transcript whose call arrived with no id is answered with the
    /// id the deserializer gave it, so the error result inserted for a dangling
    /// call answers a real id.
    #[test]
    fn a_dangling_call_with_no_id_is_answered_with_one() {
        let mut messages: Vec<Message> = serde_json::from_str(
            r#"[
                 {"role":"system","content":"you are mush"},
                 {"role":"user","content":"task"},
                 {"role":"assistant","tool_calls":[
                    {"type":"function","function":{"name":"read_file","arguments":"{}"}}
                 ]}
               ]"#,
        )
        .unwrap();
        repair_tool_pairs(&mut messages);

        assert_eq!(roles(&messages), ["system", "user", "assistant", "tool"]);
        let id = messages[2].tool_calls()[0].id.clone();
        assert_eq!(id, "call_0", "the deserializer named the call");
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

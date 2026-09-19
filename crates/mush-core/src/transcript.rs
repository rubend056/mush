//! The transcript algebra: the rules that decide the *shape* of a request.
//!
//! Pairing tool calls with their results, repairing arguments a model sent as
//! something other than JSON, dropping the oldest turns to fit a budget,
//! cutting a stored copy to the bytes a file may carry, and deciding when a
//! conversation should be folded into a summary. Nothing here calls a model or
//! touches an actor — these are pure functions over `Message`s, which is why
//! they live in core and not in the run loop that applies them.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
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

/// What a cap cut away, oldest first: how many messages, and the serialized
/// bytes they cost.
///
/// The unit is what `serde_json` writes for a message — what the file actually
/// pays — not [`Message::weight`], which is the token proxy the context budget
/// counts. The two disagree by design: weight estimates what an endpoint will
/// charge for re-sending a transcript, while a file pays for every character
/// verbatim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dropped {
    pub messages: usize,
    pub bytes: usize,
}

/// Keep the newest segments of a transcript that fit `cap`, dropping whole
/// ones out of the middle, and say what went.
///
/// A segment opens at a `user` message or at an `assistant` message: a tool
/// result can only legally follow the assistant turn it answers, so a cut just
/// before either opening can never fall between a call and its results — the
/// pairing rules are [`repair_tool_pairs`]'s. The one shape that can still
/// strand a result is the UI's, where the human typed while a tool batch ran
/// and a steering line sits between a call and its answers; a result whose call
/// went with the dropped middle goes with it.
///
/// The opening segment is never dropped, however small the cap: it is the brief
/// a child was spawned with, or the human's first words on the root, and
/// `agent::revive` only re-seeds a brief when the transcript is empty — losing
/// it would leave a revived child knowing where it got to and not what it was
/// asked to do. [`trim_history`] keeps the same minimum shape (system, task,
/// newest turn). The newest segment survives too, even when it alone is over
/// the cap, so the cap is a floor on what the middle gives up rather than a
/// byte-exact ceiling.
///
/// Idempotent: capping what capping produced drops nothing further, so a save
/// repeated on an unchanged conversation cannot keep shrinking it.
pub fn cap_transcript(messages: &mut Vec<Message>, cap: usize) -> Dropped {
    // Each message's cost, once: the greedy walk below asks for the same
    // segment repeatedly, and re-serializing to answer it would make the cut
    // quadratic in the transcript.
    let cost: Vec<usize> = messages.iter().map(stored_bytes).collect();
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == "user" || message.role == "assistant")
        .map(|(index, _)| index)
        .collect();
    if starts.len() < 2 {
        // One opening segment — perhaps with stranded results in front of it —
        // is all there is: no newer segment exists to keep in its place, so the
        // cap has nothing it could safely give up.
        return Dropped::default();
    }
    // Everything before the second segment opens is the part that stays. It is
    // normally one message, and it can only be longer if the transcript opens
    // with results nobody ever answered.
    let head_end = starts[1];
    // From the newest segment backwards, keep every whole one that fits.
    // `round > 1` is what protects the opening segment: `keep_from` can reach
    // `starts[1]` and no further, and the messages before it are never in the
    // drained range.
    let newest = starts.len() - 1;
    let mut keep_from = starts[newest];
    let mut kept: usize = cost[keep_from..].iter().sum();
    let mut round = newest;
    while round > 1 {
        let start = starts[round - 1];
        let segment: usize = cost[start..keep_from].iter().sum();
        if kept + segment > cap {
            break;
        }
        kept += segment;
        keep_from = start;
        round -= 1;
    }
    let mut dropped = Dropped {
        messages: keep_from - head_end,
        bytes: cost[head_end..keep_from].iter().sum(),
    };
    if dropped.messages == 0 {
        return dropped;
    }
    // A result whose call sat in the dropped middle is an orphan on the next
    // request — the invalid shape [`repair_tool_pairs`] exists to clean up —
    // because the UI's transcript can put the human's steering *inside* a tool
    // batch. The cut owns the damage it does: an answer whose question is gone
    // goes with it. A result whose call was never there at all is left alone;
    // that orphan predates the cut and is not this function's to repair.
    let dropped_calls: HashSet<String> = messages[head_end..keep_from]
        .iter()
        .flat_map(|message| message.tool_calls().iter().map(|call| call.id.clone()))
        .collect();
    messages.drain(head_end..keep_from);
    if !dropped_calls.is_empty() {
        let mut orphans = Dropped::default();
        messages.retain(|message| {
            let orphan = message.role == "tool"
                && message
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| dropped_calls.contains(id));
            if orphan {
                orphans.messages += 1;
                orphans.bytes += stored_bytes(message);
            }
            !orphan
        });
        dropped.messages += orphans.messages;
        dropped.bytes += orphans.bytes;
    }
    dropped
}

/// What one message costs the stored file, in bytes.
///
/// Its own JSON, compactly: the pretty form the file is written in spends a few
/// dozen bytes per message on indentation and newlines on top — measured at
/// 1.03x of a real session, because the payload is long strings and not
/// structure — and the message's own bytes are what the cap is about. Nothing
/// here re-uses [`Message::weight`]: that number is a token estimate, and a
/// stored tool result pays for its exact text.
fn stored_bytes(message: &Message) -> usize {
    serde_json::to_vec(message)
        .map(|json| json.len())
        .unwrap_or(0)
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

    /// A round: the user line that opens it and the assistant answer. The body
    /// is sized so a cap can be aimed at whole rounds instead of at whatever
    /// the JSON happens to weigh.
    fn round(index: usize, bytes: usize) -> Vec<Message> {
        vec![
            Message::user(format!("ask {index} {}", "x".repeat(bytes))),
            Message::assistant("ok"),
        ]
    }

    /// What a slice of a transcript costs the file, counted the way the cap
    /// counts.
    fn cost(messages: &[Message]) -> usize {
        messages.iter().map(stored_bytes).sum()
    }

    /// The cap keeps the newest segments and gives up the middle, so what the
    /// model just did survives while the opening task stays where it was: the
    /// stored transcript is the task and the newest work, not a stranger in the
    /// middle of somebody else's conversation.
    #[test]
    fn a_cap_keeps_the_newest_segments_and_drops_the_middle() {
        let mut messages: Vec<Message> = (0..5).flat_map(|i| round(i, 200)).collect();
        // Room for the last two rounds exactly, so the third has to go whole
        // rather than leaving an answer with no question above it.
        let two = cost(&messages[6..]);
        let middle = cost(&messages[1..6]);
        let cut = cap_transcript(&mut messages, two);

        assert_eq!(messages.len(), 5, "the task and the last two rounds");
        assert!(
            messages[0].text().starts_with("ask 0"),
            "{}",
            messages[0].text()
        );
        assert!(messages[1].text().starts_with("ask 3"));
        assert!(messages[3].text().starts_with("ask 4"));
        assert_eq!(messages.last().unwrap().text(), "ok");
        assert_eq!(cut.messages, 5, "the middle went");
        assert_eq!(cut.bytes, middle);
    }

    /// A cut between segments cannot separate a call from the results that
    /// answer it: an assistant message opens the segment its results follow it
    /// into, so both sides of the seam stay whole.
    #[test]
    fn a_cap_cut_keeps_a_call_and_its_results_together() {
        let mut messages = vec![Message::user("the brief")];
        for i in 0..4 {
            let id = format!("call{i}");
            messages.push(assistant_calling(&[&id]));
            messages.push(Message::tool(id, format!("result {i}")));
            messages.push(Message::assistant(format!("note {i}")));
        }
        // Room for the newest call-plus-result pair and the note after it.
        let newest = cost(&messages[messages.len() - 3..]);
        let cut = cap_transcript(&mut messages, newest);
        assert!(cut.messages > 0, "the fixture must actually be cut");

        assert_eq!(messages.len(), 4, "the brief and the newest pair");
        assert_eq!(messages[0].text(), "the brief", "the task stays");
        // Every call that survived has its answer, and every answer its call.
        let calls: Vec<String> = messages
            .iter()
            .flat_map(|message| message.tool_calls().iter().map(|call| call.id.clone()))
            .collect();
        let answered: Vec<String> = messages
            .iter()
            .filter_map(|message| message.tool_call_id.clone())
            .collect();
        assert_eq!(calls, answered);
        assert_eq!(calls, ["call3"]);
    }

    /// The UI lets the human type while a tool batch runs, so a stored
    /// transcript can put a user line between a call and its results. Dropping
    /// the segment with the call must take the answers with it, or the next
    /// request carries an orphan result — the invalid shape
    /// `repair_tool_pairs` exists to clean up.
    #[test]
    fn a_cut_through_an_interleaved_batch_takes_the_lost_calls_results_with_it() {
        let mut messages = vec![
            Message::user("the task"),
            assistant_calling(&["x"]),
            Message::user("steer"),
            Message::tool("x", "result x"),
            Message::assistant("done"),
        ];
        // Room for the newest segment and the steering that opens it, so the
        // segment holding call `x` is the one that goes.
        let newest = cost(&messages[2..]);
        let cut = cap_transcript(&mut messages, newest);

        assert_eq!(roles(&messages), ["user", "user", "assistant"]);
        assert_eq!(messages[0].text(), "the task");
        assert_eq!(messages[1].text(), "steer");
        assert_eq!(messages[2].text(), "done");
        assert_eq!(cut.messages, 2, "the call and its orphaned answer");
    }

    /// A result whose call was never in the transcript is not the cut's to
    /// repair: the cap moves what it must and leaves the rest of the shape to
    /// `repair_tool_pairs`, which is the door every adopted transcript comes
    /// through.
    #[test]
    fn a_pre_existing_orphan_is_not_the_cut_s_to_remove() {
        let mut messages = vec![
            Message::tool("ghost", "answered nothing"),
            Message::user("the task"),
            Message::assistant("ok"),
        ];
        let newest = cost(&messages[1..]);
        let cut = cap_transcript(&mut messages, newest);

        assert_eq!(cut, Dropped::default());
        assert_eq!(roles(&messages), ["tool", "user", "assistant"]);
    }

    /// Re-saving a truncated transcript must not keep shrinking it: the second
    /// cut finds the newest segments already fitting the cap and gives up
    /// nothing.
    #[test]
    fn capping_an_already_capped_transcript_changes_nothing() {
        let mut messages: Vec<Message> = (0..6).flat_map(|i| round(i, 300)).collect();
        let cap = cost(&messages[8..]);
        let first = cap_transcript(&mut messages, cap);
        assert!(first.messages > 0, "the fixture must actually be cut");
        let once = serde_json::to_string(&messages).unwrap();

        let again = cap_transcript(&mut messages, cap);
        assert_eq!(again, Dropped::default(), "nothing left to drop");
        assert_eq!(serde_json::to_string(&messages).unwrap(), once);
    }

    /// Nothing but tool results has no segment boundary a cut could take
    /// without stranding one on its own, so it is stored whole however small
    /// the cap.
    #[test]
    fn a_transcript_of_only_tool_results_is_never_cut() {
        let mut messages = vec![
            Message::tool("a", "result a"),
            Message::tool("b", "result b"),
        ];
        let before = serde_json::to_string(&messages).unwrap();
        let cut = cap_transcript(&mut messages, 1);

        assert_eq!(cut, Dropped::default());
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);
    }

    /// The newest segment and the opening one are never dropped, even when the
    /// newest alone is over the cap: a file that stores nothing the model just
    /// said is worse than one a segment over its bound, and the opening message
    /// is the task a revived child would otherwise have to guess at —
    /// `agent::revive` only seeds a brief when the transcript is empty.
    #[test]
    fn the_newest_segment_and_the_task_survive_a_cap_they_exceed() {
        let mut messages: Vec<Message> = (0..3).flat_map(|i| round(i, 200)).collect();
        let cut = cap_transcript(&mut messages, 1);

        assert_eq!(cut.messages, 4);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].text().starts_with("ask 0"), "the task stays");
        assert_eq!(messages[1].text(), "ok", "the newest answer stays");
        assert!(cost(&messages) > 1, "over the cap rather than emptied");
    }
}

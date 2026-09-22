//! The transcript algebra: the rules that decide the *shape* of a request.
//!
//! Pairing tool calls with their results, repairing arguments a model sent as
//! something other than JSON, shedding image payloads and dropping the oldest
//! turns to fit a budget (and saying so in the request), and deciding when a
//! conversation should be folded into a summary. Nothing here calls a model or
//! touches an actor — these are pure functions over `Message`s, which is why
//! they live in core and not in the run loop that applies them.

use serde_json::Value;

use std::collections::HashSet;

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

/// The history size at which compaction fires: nine tenths of the budget, in
/// bytes.
///
/// `budget_bytes` is the same unit [`Message::weight`] counts and the same unit
/// [`Config::history_budget`](crate::config::Config::history_budget) returns —
/// the bytes-per-token conversion lives there, once, and is saturating too. The
/// multiply saturates rather than wrapping, because nothing downstream can
/// tell a budget that was never converted from one that was: `usize::MAX` from
/// a caller that forgot the conversion (or a window a hostile endpoint
/// advertised) would otherwise wrap `* 9` down to nearly nothing and mark a
/// two-message transcript as needing a fold.
///
/// Nine tenths rather than three quarters, re-tuned on the human's numbers: the
/// fold replaces the conversation with a summary the model then works from, so
/// it should happen as late as the request that asks for it still fits — the
/// last tenth is the room that request needs for its own instruction
/// (`docs/findings.md` §8.30).
pub fn compaction_trigger(budget_bytes: usize) -> usize {
    budget_bytes.saturating_mul(9) / 10
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
///
/// The other direction of the same pair is repaired too: a result whose call is
/// not there. A second result for a call already answered, or one whose id names
/// a call in another batch, is a shape a strict server rejects and is dropped
/// ([`drop_orphan_results`]). But a result whose id names no call *anywhere* is
/// the legacy shape of the same pair: a session saved before every call had an
/// id (`1b70096`) holds `tool_call_id: ""` beside a call the deserializer has
/// since named `call_0`. Re-pointing it keeps the model's real output, where
/// dropping it would answer the call with a made-up "no result was recorded".
pub fn repair_tool_pairs(messages: &mut Vec<Message>) {
    // Every call id the transcript names, so a result can tell "the batch that
    // asked for me is gone" (its id is known, somewhere) from "I was saved
    // before ids existed" (no batch names it).
    let known: HashSet<String> = messages
        .iter()
        .flat_map(|message| message.tool_calls().iter().map(|call| call.id.clone()))
        .collect();
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
        // The block this batch owns: everything up to the next assistant
        // message. Results pulled in here belong immediately after the batch;
        // anything else that sat in between (a nudge, usually) keeps its order
        // after them — which is where the actor folded it at runtime.
        let mut end = index + 1;
        while end < messages.len() && messages[end].role != "assistant" {
            end += 1;
        }
        // Which message answers each call, read-only so the moves below cannot
        // re-index the answer: a result that names the call, in block order —
        // a call already answered by an earlier duplicate stays unanswered
        // here — and then, for calls still unanswered, a result whose id no
        // batch names, in block order. That second pass is last so a real,
        // id-carrying result always outranks a legacy one for the same call.
        let mut source: Vec<Option<usize>> = vec![None; calls.len()];
        let block = index + 1..end;
        for (offset, message) in messages[block.clone()].iter().enumerate() {
            if message.role != "tool" {
                continue;
            }
            let named = message
                .tool_call_id
                .as_deref()
                .and_then(|id| calls.iter().position(|call| call == id))
                .filter(|at| source[*at].is_none());
            if let Some(at) = named {
                source[at] = Some(block.start + offset);
            }
        }
        for (offset, message) in messages[block.clone()].iter().enumerate() {
            let cursor = block.start + offset;
            if message.role != "tool" || source.contains(&Some(cursor)) {
                continue;
            }
            let unclaimed = !message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| known.contains(id));
            if !unclaimed {
                continue;
            }
            let Some(at) = source.iter().position(Option::is_none) else {
                break;
            };
            source[at] = Some(cursor);
        }
        // Rebuild the block: each call's result in call order (re-pointed at
        // the call it answers, so a legacy result carries a real id), a call
        // with no result answered with an explicit error the model can act on
        // (the run was interrupted mid-batch), then every other message in its
        // original order — including an unmatched result, which
        // [`drop_orphan_results`] takes out.
        let mut rebuilt: Vec<Message> = Vec::with_capacity(end - index - 1);
        for (at, id) in calls.iter().enumerate() {
            match source[at] {
                Some(cursor) => {
                    let mut result = messages[cursor].clone();
                    result.tool_call_id = Some(id.clone());
                    rebuilt.push(result);
                }
                None => rebuilt.push(Message::tool(
                    id.clone(),
                    "error: no result was recorded for this call (the run was interrupted)",
                )),
            }
        }
        for (offset, message) in messages[block.clone()].iter().enumerate() {
            if !source.contains(&Some(block.start + offset)) {
                rebuilt.push(message.clone());
            }
        }
        let next = index + 1 + rebuilt.len();
        messages.splice(index + 1..end, rebuilt);
        index = next;
    }
    drop_orphan_results(messages);
}

/// Keep only the `tool` messages that answer the batch right above them.
///
/// [`repair_tool_pairs`] guarantees every call has a result; this guarantees
/// the other direction. A second result for one call, or one whose id names a
/// call in an earlier batch that arrived too late to answer it, is what a
/// hand-edited file grows; both are rejected by a strict server as firmly as a
/// dangling call, and neither can be explained to a model that cannot see the
/// call it names. Each accepted result consumes its call, so a duplicate has
/// nothing left to answer. (A result whose id names no batch at all has already
/// been re-pointed by [`repair_tool_pairs`] if a call was waiting for it.)
fn drop_orphan_results(messages: &mut Vec<Message>) {
    let mut batch: Vec<String> = Vec::new();
    let mut kept: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        if message.role == "assistant" {
            batch = message
                .tool_calls()
                .iter()
                .map(|call| call.id.clone())
                .collect();
        } else if message.role == "tool" {
            let answered = message
                .tool_call_id
                .as_deref()
                .and_then(|id| batch.iter().position(|call| call == id));
            match answered {
                Some(at) => {
                    batch.remove(at);
                }
                None => continue,
            }
        } else {
            batch.clear();
        }
        kept.push(message);
    }
    *messages = kept;
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

/// The one line a request carries when trimming had to drop the oldest turns:
/// without it a model continues as if it held the whole conversation and can
/// contradict a fact it "already read", with nothing to say why the fact is
/// gone. The note travels in the request only — the vec [`trim_history`] trims
/// is the actor's working copy, while the copy a session stores is the UI's,
/// which learns a line only from an emitted [`Message`] event — so it is not
/// accumulated in `.mush/session.json`.
///
/// It speaks in the user's voice, the voice mush's other out-of-band notes use
/// (`COMPACT_INSTRUCTION`, `TRUNCATION_INSTRUCTION`, a folded completion): the
/// assistant's would be a fabricated turn, and a thinking endpoint refuses a
/// replayed assistant turn that carries no `reasoning_content`. [`trim_history`]
/// keeps it out of the `user_indices` arithmetic, which counts user lines as
/// turn boundaries.
const DROPPED_TURNS_NOTE: &str = "\
The oldest turns of this conversation were dropped to fit the context window, \
so this transcript is not the whole conversation: a fact you cannot find here \
may have been dropped rather than never said.";

/// Drop the oldest turns until the conversation fits the budget. Trimming at a
/// user message keeps assistant/tool pairs intact, which servers validate.
/// The budget comes from the endpoint's context window.
///
/// Image payloads are shed first, oldest message first, each replaced by the
/// placeholder that names its path ([`Message::drop_images`]). An image is what
/// an over-budget transcript is usually made of, and the cheapest thing to
/// lose: the placeholder says which file it came from, so the model can read it
/// again if it needs it, where a dropped turn is gone with its words. Only once
/// no image is left does the drain below start dropping turns — which run still
/// happens when the words alone are over budget.
///
/// A transcript that lost turns says so once, in `DROPPED_TURNS_NOTE`'s line.
/// The note is built here, counted against the budget like any other message,
/// and kept out of the draining below — a `user` line would otherwise read as
/// a turn boundary — and a later drain replaces it along with the turns it was
/// explaining. A transcript that only lost image payloads carries no note: its
/// turns are all there, and the placeholders say what happened.
pub fn trim_history(messages: &mut Vec<Message>, budget: usize) {
    // Whatever an earlier call left comes out first: the arithmetic below
    // counts `user` lines as turns, and the note is not one.
    let carried = messages.get(2).is_some_and(is_dropped_note);
    if carried {
        messages.remove(2);
    }
    let note = Message::user(DROPPED_TURNS_NOTE);
    let mut dropped = false;
    loop {
        // Once a drop happens the request will carry the note, so the budget
        // has to hold the note too, or the line explaining the trim would be
        // what pushes the request past the window. The sum saturates: a header
        // can claim a picture whose estimate is as large as a `usize`
        // (`Image::weight`), and a transcript of them has to read as over
        // budget rather than wrap to a small number that says it fits.
        let total: usize = messages
            .iter()
            .map(Message::weight)
            .fold(0, usize::saturating_add)
            .saturating_add(if carried || dropped { note.weight() } else { 0 });
        if total <= budget {
            break;
        }
        // The bytes go before the words. Each pass sheds the oldest message
        // still carrying one, so the replay below re-weighs what is left — the
        // placeholder the drop leaves is counted like any other text — and only
        // a transcript with no images left reaches the drain.
        if let Some(message) = messages
            .iter_mut()
            .find(|message| !message.images.is_empty())
        {
            message.drop_images();
            continue;
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
            break;
        }
        // Drop the oldest full turn: everything after the task message up to
        // the third user message, cutting at user boundaries so pairs stay
        // valid. Guarded so the drain can never be a no-op (which would spin
        // here forever) on a transcript that does not start with system+user.
        let keep_from = user_indices[2];
        if keep_from <= 2 {
            break;
        }
        messages.drain(2..keep_from);
        dropped = true;
    }
    if carried || dropped {
        // Where the dropped turns were: after the system prompt and the
        // opening task, before the oldest turn that was kept.
        messages.insert(2, note);
    }
}

/// Whether a message is the note [`trim_history`] leaves behind when it drops
/// turns. One shape, compared in the one place that has to tell the note from
/// a turn.
fn is_dropped_note(message: &Message) -> bool {
    message.role == "user" && message.text() == DROPPED_TURNS_NOTE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::{FunctionCall, Image, ToolCall};

    /// A one-message transcript weighing exactly `weight` bytes. `Message::weight`
    /// counts the role plus the text, so the text is sized to land on the
    /// number instead of being padded until the assertion happens to hold.
    fn transcript_of_weight(weight: usize) -> Vec<Message> {
        vec![Message::user("x".repeat(weight - "user".len()))]
    }

    /// The trigger is computed, not written a second time: nine tenths of the
    /// budget, saturating.
    #[test]
    fn the_trigger_is_nine_tenths_of_the_budget() {
        assert_eq!(compaction_trigger(0), 0);
        assert_eq!(compaction_trigger(1_000), 900);
        assert_eq!(compaction_trigger(7_501), 6_750);
    }

    /// A budget that is not bytes at all — `usize::MAX`, what a caller that
    /// skipped the bytes-per-token conversion in `Config::history_budget`
    /// hands over — must neither panic nor lie. `budget * 3 / 4` panicked here
    /// in a debug build, and in a release one it wrapped: at
    /// `6_148_914_691_236_517_206` three times the budget wraps to 2, so the
    /// old expression returned 0 and marked *every* transcript, however small,
    /// as needing a fold. Saturating, the trigger stays within a tenth of the
    /// budget and an ordinary transcript is nowhere near it.
    #[test]
    fn a_nonsense_budget_does_not_wrap_the_trigger_to_zero() {
        assert_eq!(compaction_trigger(usize::MAX), usize::MAX / 10);
        assert_eq!(
            compaction_trigger(6_148_914_691_236_517_206),
            usize::MAX / 10
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

    /// How many copies of the note a transcript carries.
    fn note_count(messages: &[Message]) -> usize {
        messages
            .iter()
            .filter(|message| message.text() == DROPPED_TURNS_NOTE)
            .count()
    }

    /// A long transcript as a run builds one: the opening system+task pair and
    /// then `turns` user turns, each with an assistant reply and a tool result.
    fn long_transcript(turns: usize) -> Vec<Message> {
        let mut messages = vec![Message::system("you are mush"), Message::user("first")];
        for i in 0..turns {
            messages.push(Message::assistant(format!("reply {i} {}", "x".repeat(500))));
            messages.push(Message::tool(format!("call{i}"), "result"));
            messages.push(Message::user(format!("again {i}")));
        }
        messages
    }

    /// A drain is not silent: the request carries one line saying the oldest
    /// turns were dropped, where they used to be — after the system prompt and
    /// the opening task — so a model cannot read a fact out of a transcript
    /// that no longer holds it.
    #[test]
    fn a_trimmed_request_says_the_oldest_turns_were_dropped() {
        let mut messages = long_transcript(50);
        trim_history(&mut messages, 8_000);

        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user", "the opening task survives");
        assert_eq!(messages[2].text(), DROPPED_TURNS_NOTE);
        assert_eq!(
            messages[2].role, "user",
            "the note is a user line, and it is not counted as a turn"
        );
        assert_eq!(note_count(&messages), 1, "one line, however much was cut");
        assert!(
            messages.iter().map(Message::weight).sum::<usize>() <= 8_000,
            "the explanation fits the budget it explains"
        );
    }

    /// A second drain on the same transcript replaces the line and keeps its
    /// place: a long run carries exactly one note about what it lost, never a
    /// pile of them.
    #[test]
    fn a_second_drain_replaces_the_note_instead_of_stacking_it() {
        let mut messages = long_transcript(50);
        trim_history(&mut messages, 8_000);
        assert_eq!(note_count(&messages), 1);

        for i in 50..100 {
            messages.push(Message::assistant(format!("reply {i} {}", "x".repeat(500))));
            messages.push(Message::tool(format!("call{i}"), "result"));
            messages.push(Message::user(format!("again {i}")));
        }
        trim_history(&mut messages, 8_000);
        assert_eq!(note_count(&messages), 1, "replaced, not stacked");
        assert_eq!(messages[2].text(), DROPPED_TURNS_NOTE, "and still in place");
        assert_eq!(messages[1].role, "user");
        assert!(messages.iter().map(Message::weight).sum::<usize>() <= 8_000);
    }

    /// A trim that can cut no further keeps the line it already carries:
    /// there is no second note and no lost explanation.
    #[test]
    fn a_note_is_carried_when_a_later_trim_can_cut_no_further() {
        let mut messages = long_transcript(50);
        trim_history(&mut messages, 8_000);
        assert_eq!(note_count(&messages), 1);
        // Below the minimum shape's own weight: the drain reaches system +
        // task + one turn and stops, and the note has to survive that.
        trim_history(&mut messages, 0);
        assert_eq!(note_count(&messages), 1, "kept, not lost or stacked");
        assert_eq!(messages[2].text(), DROPPED_TURNS_NOTE);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
    }

    /// Nothing dropped means nothing said: a transcript that already fits is
    /// not annotated, and neither is one the trimmer cannot cut (a shape with
    /// no pair to keep) — the note is a fact about what happened, not a hedge.
    #[test]
    fn an_untouched_transcript_carries_no_note() {
        let mut messages = vec![Message::system("you are mush"), Message::user("task")];
        let before = serde_json::to_string(&messages).unwrap();
        trim_history(&mut messages, 10_000);
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);

        let mut messages = vec![Message::user("a"), Message::user("b"), Message::user("c")];
        trim_history(&mut messages, 0);
        assert_eq!(messages.len(), 3, "nothing can be trimmed without a pair");
        assert_eq!(note_count(&messages), 0);

        // Exactly two turns and over budget: the guard refuses the drain, so
        // there is no drop to explain and no note to add either.
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("first"),
            Message::assistant("done"),
            Message::user("x".repeat(10_000)),
        ];
        trim_history(&mut messages, 100);
        assert_eq!(messages.len(), 4);
        assert_eq!(note_count(&messages), 0);
    }

    /// An image of `bytes` bytes whose path says which one it is, for the trim
    /// tests: big enough to sway a budget, small enough to read. No pixels, so
    /// it weighs its bytes — the fallback those tests exercise.
    fn image(path: &str, bytes: usize) -> Image {
        Image {
            path: path.into(),
            mime: "image/png".into(),
            bytes: vec![0x41; bytes],
            pixels: None,
        }
    }

    /// A picture `width × height` big stored in `bytes` bytes: what the budget
    /// weighs by its pixels, whatever the file happens to be.
    fn picture(path: &str, width: u32, height: u32, bytes: usize) -> Image {
        Image {
            pixels: Some((width, height)),
            ..image(path, bytes)
        }
    }

    /// A transcript over budget *only because of an image* keeps every turn:
    /// the payload is shed first, the placeholder names the path so the image
    /// can be read again, and no note is written because no turn was dropped.
    /// Without that order the drain would have thrown away the turn the image
    /// arrived in — losing the model's words to protect the endpoint from bytes
    /// that are exactly what a placeholder can stand in for.
    #[test]
    fn an_image_is_dropped_before_any_turn_is() {
        let mut messages = vec![Message::system("you are mush"), Message::user("first")];
        let mut reply = Message::assistant("here it is");
        reply.images.push(image("shots/huge.png", 40_000));
        messages.push(reply);
        messages.push(Message::tool("call_0", "rendered"));
        messages.push(Message::user("next"));
        messages.push(Message::assistant("done"));
        messages.push(Message::user("last"));
        let before: Vec<String> = messages.iter().map(|m| m.role.clone()).collect();
        let words: usize = messages
            .iter()
            .filter(|message| message.images.is_empty())
            .map(Message::weight)
            .sum();
        let budget = words + 1_000;
        assert!(
            messages.iter().map(Message::weight).sum::<usize>() > budget,
            "the fixture is over budget because of the image, not the words"
        );

        trim_history(&mut messages, budget);

        let after: Vec<String> = messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(after, before, "every turn survives the trim");
        assert!(messages[2].images.is_empty(), "the payload is what went");
        assert!(
            messages[2].text().contains("shots/huge.png"),
            "the placeholder names the path: {}",
            messages[2].text()
        );
        assert_eq!(
            note_count(&messages),
            0,
            "no turn was dropped, so the note about dropped turns would be a lie"
        );
        assert!(
            messages.iter().map(Message::weight).sum::<usize>() <= budget,
            "the transcript now fits"
        );
    }

    /// Shedding an image is not an escape from the drain: when the words alone
    /// are over budget, turns drop exactly as they did before images existed,
    /// and no payload survives to be sent.
    #[test]
    fn the_turn_drain_still_runs_when_the_words_alone_are_over_budget() {
        let mut messages = long_transcript(50);
        for (i, message) in messages.iter_mut().enumerate() {
            if message.role == "assistant" {
                message.images.push(image(&format!("shots/{i}.png"), 200));
            }
        }
        trim_history(&mut messages, 8_000);

        assert_eq!(messages[2].text(), DROPPED_TURNS_NOTE);
        assert!(
            messages.iter().all(|message| message.images.is_empty()),
            "no payload survives a trim that had to drop turns"
        );
        assert!(messages.iter().map(Message::weight).sum::<usize>() <= 8_000);
    }

    /// Oldest first, and only as many as the budget needs: the payload the
    /// transcript has carried the longest is what goes, and a newer image is
    /// not thrown away for room the older one's drop already made.
    #[test]
    fn the_oldest_image_payload_is_shed_before_a_newer_one() {
        let mut messages = vec![Message::system("you are mush"), Message::user("first")];
        for (i, path) in ["shots/one.png", "shots/two.png"].into_iter().enumerate() {
            let mut reply = Message::assistant(format!("here {i}"));
            reply.images.push(image(path, 7_000));
            messages.push(reply);
            messages.push(Message::user(format!("again {i}")));
        }
        // Room for the words and one of the two payloads, not both.
        let budget = 8_000;
        assert!(messages.iter().map(Message::weight).sum::<usize>() > budget);

        trim_history(&mut messages, budget);

        assert!(
            messages[2].images.is_empty(),
            "the oldest message with an image is the one that pays"
        );
        assert_eq!(
            messages[4].images.len(),
            1,
            "the newer payload stays: the budget can hold it"
        );
        assert!(
            messages[2].text().contains("shots/one.png"),
            "the placeholder names the older path: {}",
            messages[2].text()
        );
        assert_eq!(note_count(&messages), 0, "no turn was dropped");
        assert!(messages.iter().map(Message::weight).sum::<usize>() <= budget);
    }

    /// The other half of the pixels ruling: the estimate changed, the order
    /// did not. A transcript over budget because of pixel-weighed pictures
    /// still sheds the oldest one first, and keeps the newer one the budget
    /// can hold.
    #[test]
    fn a_picture_over_budget_is_still_shed_oldest_first() {
        let mut messages = vec![Message::system("you are mush"), Message::user("first")];
        for (i, path) in ["shots/one.png", "shots/two.png"].into_iter().enumerate() {
            let mut reply = Message::assistant(format!("here {i}"));
            reply.images.push(picture(path, 1_920, 1_080, 700));
            messages.push(reply);
            messages.push(Message::user(format!("again {i}")));
        }
        // Room for the words and one 1920×1080 screenshot (~8.3 KB of weight
        // each), not both.
        let budget = 10_000;
        assert!(messages.iter().map(Message::weight).sum::<usize>() > budget);

        trim_history(&mut messages, budget);

        assert!(
            messages[2].images.is_empty(),
            "the oldest picture is the one that pays"
        );
        assert_eq!(
            messages[4].images.len(),
            1,
            "the newer picture stays: the budget can hold it"
        );
        assert!(messages[2].text().contains("shots/one.png"));
        assert_eq!(note_count(&messages), 0, "no turn was dropped");
        assert!(messages.iter().map(Message::weight).sum::<usize>() <= budget);
    }

    /// The human's own numbers, and the defect this fixes: a ~300k-token
    /// conversation (~900 KB of weight) plus a 724 KiB 1920×1080 screenshot
    /// fits a 500k-token window's budget, because the picture costs ~2.8k
    /// tokens by its pixels where its bytes read as ~247k. The screenshot they
    /// had just attached is not shed before the model ever looks at it.
    #[test]
    fn the_humans_screenshot_fits_and_is_not_shed() {
        let mut cfg = Config::new("http://127.0.0.1:1", "test", None);
        cfg.set_context(500_000);
        let budget = cfg.history_budget();

        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("first"),
            Message::assistant("x".repeat(900_000)),
        ];
        let words: usize = messages.iter().map(Message::weight).sum();
        messages.last_mut().unwrap().images.push(picture(
            "shots/screen.png",
            1_920,
            1_080,
            741_396,
        ));
        let total: usize = messages.iter().map(Message::weight).sum();

        assert!(
            words + 741_396 > budget,
            "the old byte count put the fixture over budget: {} > {budget}",
            words + 741_396
        );
        assert!(
            total <= budget,
            "the pixels count fits it: {total} > {budget}"
        );

        trim_history(&mut messages, budget);

        assert_eq!(
            messages.last().unwrap().images.len(),
            1,
            "the newest image is not shed before the model can see it"
        );
        assert_eq!(note_count(&messages), 0, "and no turn was dropped either");
    }

    /// The placeholder weighs its own text and nothing else: once a payload is
    /// shed, neither the pixels nor the bytes are in the budget any more. A
    /// trimmer that still counted them would shed the same image forever and
    /// never get under a budget its picture is no longer part of.
    #[test]
    fn a_shed_payload_leaves_only_the_placeholders_weight() {
        let mut message = Message::assistant("here it is");
        message
            .images
            .push(picture("shots/screen.png", 1_920, 1_080, 741_396));
        let with_image = message.weight();

        message.drop_images();

        let text = message.text().to_string();
        assert_eq!(
            message.weight(),
            "assistant".len() + text.len(),
            "role and placeholder text, and no payload: {text}"
        );
        assert!(message.weight() < with_image, "the picture is gone");
        assert!(
            with_image - message.weight() > 8_000,
            "and its ~8.3 KB of weight with it: {with_image} -> {}",
            message.weight()
        );
        assert!(
            text.contains("shots/screen.png"),
            "and the path that replaces it is there: {text}"
        );
    }

    /// A transcript of impossible pictures is over budget, not wrapped under
    /// it: 260 headers that each claim `u32::MAX × u32::MAX` pixels sum past a
    /// `usize`, and the saturating total still says "shed" — a plain sum would
    /// panic in a debug build and wrap to a small, fitting-looking number in a
    /// release one.
    #[test]
    fn a_transcript_of_impossible_pictures_still_reads_as_over_budget() {
        let mut messages = vec![Message::system("you are mush"), Message::user("first")];
        for i in 0..260 {
            let mut reply = Message::assistant(format!("here {i}"));
            reply.images.push(Image {
                path: format!("shots/{i}.png"),
                mime: "image/png".into(),
                bytes: vec![],
                pixels: Some((u32::MAX, u32::MAX)),
            });
            messages.push(reply);
        }

        trim_history(&mut messages, 1_000);

        assert!(
            messages.iter().all(|message| message.images.is_empty()),
            "every payload is shed, so the total was read as over budget"
        );
        assert!(messages.len() > 2, "the words are not what was dropped");
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

    /// The other half of a stored pair: a result whose call is not there. A
    /// session saved before every call had an id holds `tool_call_id: ""`
    /// beside a call the deserializer has since named `call_0`; a duplicate
    /// answer is the same shape to a strict server. Neither can be explained to
    /// a model that cannot see the call, so both are dropped and the valid pair
    /// is left exactly as it was.
    #[test]
    fn a_result_whose_call_is_gone_is_dropped() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["a"]),
            Message::tool("", "the result of a call this file lost"),
            Message::tool("a", "result a"),
            Message::tool("a", "the same answer twice"),
            Message::user("next"),
        ];
        repair_tool_pairs(&mut messages);

        assert_eq!(
            roles(&messages),
            ["system", "user", "assistant", "tool", "user"]
        );
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("a"));
        assert_eq!(messages[3].text(), "result a");
    }

    /// A session saved before `1b70096` holds a result whose `tool_call_id` is
    /// the empty string beside a call the deserializer has renamed `call_0`:
    /// the same pair, with the id half missing. The result is real output and
    /// the batch is the block it was saved under, so it is re-pointed at the
    /// call waiting for it — dropping it would answer the call with a made-up
    /// "no result was recorded" while the real one sat in the file.
    #[test]
    fn a_legacy_result_with_no_id_is_re_pointed_at_its_call() {
        let mut messages: Vec<Message> = serde_json::from_str(
            r#"[
                 {"role":"system","content":"you are mush"},
                 {"role":"user","content":"task"},
                 {"role":"assistant","tool_calls":[
                    {"type":"function","function":{"name":"run_command","arguments":"{}"}}
                 ]},
                 {"role":"tool","content":"test result: ok","tool_call_id":""}
               ]"#,
        )
        .unwrap();
        repair_tool_pairs(&mut messages);

        assert_eq!(roles(&messages), ["system", "user", "assistant", "tool"]);
        let id = messages[2].tool_calls()[0].id.clone();
        assert_eq!(id, "call_0", "the deserializer named the call");
        assert_eq!(messages[3].tool_call_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            messages[3].text(),
            "test result: ok",
            "the real output is kept"
        );
    }

    /// An id a batch *does* name is never adopted by another batch's unanswered
    /// call: a result that arrived in the wrong place is dropped, not handed to
    /// the wrong call. Only an id no batch names — a session from before ids
    /// existed — is re-pointed.
    #[test]
    fn a_misplaced_result_is_not_adopted_by_another_call() {
        let mut messages = vec![
            Message::system("you are mush"),
            Message::user("task"),
            assistant_calling(&["call_1"]),
            Message::tool("call_1", "the first answer"),
            assistant_calling(&["call_2"]),
            Message::tool("call_1", "arrived one batch too late"),
            Message::user("next"),
        ];
        repair_tool_pairs(&mut messages);

        assert_eq!(
            roles(&messages),
            [
                "system",
                "user",
                "assistant",
                "tool",
                "assistant",
                "tool",
                "user"
            ]
        );
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[3].text(), "the first answer");
        assert_eq!(messages[5].tool_call_id.as_deref(), Some("call_2"));
        assert!(
            messages[5].text().starts_with("error:"),
            "the second call gets the interrupted sentence, not another call's result: {}",
            messages[5].text()
        );
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

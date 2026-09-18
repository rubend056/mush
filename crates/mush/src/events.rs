//! The event seam: how an actor reports to the UI.
//!
//! Every agent emits id-tagged events into one channel, and the UI routes them
//! into the tree and the chat. `emit` is the whole seam: the real sink stamps
//! each event with the conversation it belongs to and sends it to the UI
//! thread, and a test records them instead. That is what makes "a completion
//! was delivered exactly once", "the event landed on the agent it names" and "a
//! send to a closed UI channel is harmless" assertions rather than facts about
//! a receiver nobody reads.

use crossbeam_channel::Sender;

use crate::agent::AgentEvent;
use crate::app::{AgentId, ConversationId, Msg};

/// Where an agent's events go.
///
/// `Send + Sync` because one sink is shared by every actor in a tree, and a
/// child emits on its own thread.
pub trait Events: Send + Sync {
    /// Report what happened to `id`. Never blocks the run's own work: a sink
    /// that cannot deliver must not be able to stall an agent.
    fn emit(&self, id: AgentId, event: AgentEvent);
}

/// The real sink: the UI thread's channel, stamped with the conversation.
///
/// The stamp is what lets the UI recognise an event from a tree Ctrl-N
/// abandoned and drop it, instead of folding a finished request into the new
/// chat.
pub struct Ui {
    tx: Sender<Msg>,
    conversation: ConversationId,
}

impl Ui {
    pub fn new(tx: Sender<Msg>, conversation: ConversationId) -> Self {
        Self { tx, conversation }
    }
}

impl Events for Ui {
    fn emit(&self, id: AgentId, event: AgentEvent) {
        // The UI may be gone — the terminal was closed, the app is shutting
        // down — and a send to a closed channel must stay harmless: the agent
        // finishes its work either way, and nothing here spins on the failure.
        let _ = self.tx.send(Msg::Agent {
            conversation: self.conversation,
            id,
            event,
        });
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crossbeam_channel::{Receiver, Sender};

    use super::Events;
    use crate::agent::AgentEvent;
    use crate::app::AgentId;

    /// Records every event, in order, and can block until the next one lands.
    ///
    /// A test waits on the event it is about to assert on instead of polling,
    /// and reads the whole log when it wants to know that something happened
    /// *exactly once* — the fact a leaked receiver could never show.
    pub struct Recorder {
        events: Mutex<Vec<(AgentId, AgentEvent)>>,
        signal: (Sender<()>, Receiver<()>),
    }

    impl Recorder {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
                signal: crossbeam_channel::unbounded(),
            })
        }

        /// Everything emitted so far, oldest first.
        pub fn events(&self) -> Vec<(AgentId, AgentEvent)> {
            self.events.lock().unwrap().clone()
        }

        /// What was emitted for one agent.
        pub fn events_for(&self, id: AgentId) -> Vec<AgentEvent> {
            self.events()
                .into_iter()
                .filter(|(event_id, _)| *event_id == id)
                .map(|(_, event)| event)
                .collect()
        }

        /// How many events have been emitted at all.
        pub fn len(&self) -> usize {
            self.events.lock().unwrap().len()
        }

        /// Block until the next event lands, or `timeout` passes. Returns
        /// whether one did, so a run that stops emitting fails an assertion
        /// instead of hanging the suite.
        pub fn wait(&self, timeout: Duration) -> bool {
            self.signal.1.recv_timeout(timeout).is_ok()
        }
    }

    impl Events for Recorder {
        fn emit(&self, id: AgentId, event: AgentEvent) {
            self.events.lock().unwrap().push((id, event));
            let _ = self.signal.0.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    use super::fake::Recorder;
    use super::{Events, Ui};
    use crate::agent::AgentEvent;
    use crate::app::{AgentId, ConversationId, Msg};

    #[test]
    fn every_event_reaches_the_sink_once_in_order() {
        let recorder = Recorder::new();
        recorder.emit(AgentId(1), AgentEvent::Status("one".into()));
        recorder.emit(AgentId(1), AgentEvent::Done);
        recorder.emit(AgentId(2), AgentEvent::Stopped);

        assert_eq!(recorder.len(), 3, "no event is delivered twice");
        let for_one = recorder.events_for(AgentId(1));
        assert_eq!(for_one.len(), 2, "and each one lands on the agent it names");
        assert!(matches!(for_one[0], AgentEvent::Status(ref what) if what == "one"));
        assert!(matches!(for_one[1], AgentEvent::Done));
        assert!(recorder.wait(Duration::ZERO), "a waiter is woken by it");
    }

    /// The UI's sink stamps the conversation on every event, so a tree the
    /// human abandoned cannot land in the new chat.
    #[test]
    fn the_ui_sink_tags_every_event_with_its_conversation() {
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let ui = Ui::new(tx, ConversationId(7));
        ui.emit(
            AgentId(2),
            AgentEvent::Running {
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        match rx.try_recv() {
            Ok(Msg::Agent {
                conversation,
                id,
                event,
            }) => {
                assert_eq!(conversation, ConversationId(7));
                assert_eq!(id, AgentId(2));
                assert!(matches!(event, AgentEvent::Running { .. }));
            }
            Ok(_) => panic!("expected an agent event"),
            Err(error) => panic!("the sink sent nothing: {error}"),
        }
    }

    /// The UI is not obliged to be there. Closing the terminal mid-run must not
    /// fail an agent, and must not make the sink retry or spin.
    #[test]
    fn a_send_to_a_closed_ui_channel_is_harmless() {
        let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
        let ui = Ui::new(tx, ConversationId(1));
        drop(rx);
        ui.emit(AgentId(0), AgentEvent::Done);
        ui.emit(AgentId(0), AgentEvent::Error("after the UI left".into()));
    }

    /// A recording sink is not a UI: nothing about it needs a receiver to stay
    /// alive, which is what the leaked `ui_rx` in the actor tests was for.
    #[test]
    fn a_recording_sink_keeps_its_events_after_the_emitter_is_gone() {
        let recorder = Recorder::new();
        {
            let sink: Arc<dyn Events> = recorder.clone();
            sink.emit(AgentId(0), AgentEvent::Done);
        }
        assert!(
            matches!(recorder.events_for(AgentId(0))[..], [AgentEvent::Done]),
            "the events are still there after the emitter is gone"
        );
        assert!(recorder.wait(Duration::ZERO), "the emit woke a waiter");
        assert!(
            !recorder.wait(Duration::from_millis(1)),
            "and nothing else follows it"
        );
    }
}

//! One owner for the endpoint, the model, the key and the context window.
//!
//! The context window is the fact that made this module necessary (finding B7):
//! the bar and the tool caps read the UI's copy, while every request
//! an actor sends reads the tree's copy, and both used to be written by hand at
//! each site that learned something. A number one side learned and the other
//! did not is a screen that promises room the next request does not have.
//!
//! The invariant: every write to one of the copies goes through one of this
//! module's functions, never by hand at the site that happened to learn
//! something. A window mush did not get from the human is written in exactly
//! one place — the handle's private `ConfigHandle::learn_window` — by both
//! roads: [`ConfigCell::learn_context`], which is the UI adopting a number an
//! actor announced, and [`ConfigHandle::learn_context`], which is an actor
//! adopting what the endpoint said. Both run the one policy in [`believable`],
//! against the copy in force, and the UI's plain copy is then read back from
//! the shared cell *whatever the answer* — the number and the road it came
//! by — taken, both hold the new window; refused, both hold the window in
//! force — so a learn decision cannot leave the two apart. (Finding D21: the
//! UI used to judge an announced number against its own frame-cached copy, so a
//! handle that had already learned a smaller window left the UI refusing a
//! number that was already in force.)
//!
//! The handle takes the announcement as an argument
//! ([`ConfigHandle::learn_context`]) and calls it exactly when the number
//! landed, so a caller cannot learn a window the UI never hears about. (The one
//! way the copies can still differ is a thread that panicked while holding the
//! cell; see `ConfigHandle::adopt`.)
//!
//! Two faces, one cell: the UI thread holds a [`ConfigCell`] — a plain copy to
//! read on every frame, and the shared cell to write through — and an actor is
//! given a [`ConfigHandle`], which can read the configuration and learn a window
//! and nothing else. An actor has no business inventing a human's edit.

use std::sync::{Arc, Mutex};

pub use mush_core::config::WindowSource;
use mush_core::Config;

/// What a request fails with when another thread panicked while holding the
/// cell. One string, in one place: the actor and the transport say the same
/// thing about the same failure.
const POISONED: &str = "shared configuration poisoned";

/// Whether a window from `source` is worth adopting, against the one in use.
///
/// The one policy both faces of the cell run, so the UI cannot accept a number
/// an actor refused, or the other way round. A number means the same thing
/// whoever said it; how far it is to be trusted does not: `/v1/models` states
/// the window as a field, so it is taken as given; a *complaint* is prose mush
/// parses out of a refusal body, so it has to look plausible against the window
/// in use as well — otherwise a rate-limit body would teach mush that the
/// endpoint has ten tokens (finding A3).
///
/// The other two roads are not learnable numbers at all. `Stated` is the
/// human's own: it outranks discovery, and a caller that already holds it has
/// nothing to adopt. `Table` is mush's own assumption filling a silence — the
/// number [`Config::rederive_context`] writes — not a fact an endpoint taught,
/// so no learn call carries it.
fn believable(in_use: usize, tokens: usize, source: WindowSource) -> bool {
    match source {
        WindowSource::Stated | WindowSource::Table => false,
        WindowSource::Advertised => true,
        // A complaint is only taken when it shrinks the window without
        // collapsing it.
        WindowSource::Complaint => tokens < in_use && tokens.saturating_mul(8) >= in_use,
    }
}

/// What the UI thread holds: the configuration it reads, and the cell every
/// actor of this tree reads.
pub struct ConfigCell {
    /// A plain value, because the UI reads it on every frame: a lock per read
    /// would be a cost with nothing to buy. It is still only ever *written*
    /// through this module.
    ui: Config,
    /// The copy the actors read. Every agent of the tree was started from it,
    /// so an edit here is on the next request of every one of them.
    shared: ConfigHandle,
}

impl ConfigCell {
    /// A cell of its own, for a tree that is starting. The handle it hands out
    /// is the one the root actor is started with, so both sides begin from the
    /// same value.
    pub fn own(cfg: Config) -> Self {
        let shared = ConfigHandle::own(cfg.clone());
        Self { ui: cfg, shared }
    }

    /// The configuration every screen reads.
    pub fn ui(&self) -> &Config {
        &self.ui
    }

    /// What an actor is given.
    pub fn handle(&self) -> ConfigHandle {
        self.shared.clone()
    }

    /// Change the configuration: the UI's copy and the actors' copy in one
    /// write, so a `/model`, `/url` or `/key` cannot reach one of them and not
    /// the other (finding B7).
    ///
    /// One rule rides here because the endpoint, the key and the host are one
    /// fact: a key belongs to a host, so an edit that moves the endpoint to
    /// another host forgets the key unless the same write states a new one
    /// (findings C6, D6). Every runtime road that changes the endpoint goes
    /// through this function, so no road can leave a key aimed at the old host —
    /// and no request can be built with a key whose endpoint changed under it —
    /// and the answer says whether one was dropped, which is the one thing a
    /// caller owes the human in the acknowledgement.
    pub fn edit(&mut self, f: impl FnOnce(&mut Config)) -> bool {
        let was = self.ui.base_url.clone();
        let key_was = self.ui.api_key.clone();
        f(&mut self.ui);
        // A key stated in the same write is stated for the new endpoint: the
        // closure set it deliberately, so it is not the key that was carried
        // across the change.
        let forgotten = self.ui.api_key == key_was && self.ui.forget_key_if_host_changed(&was);
        self.shared.adopt(&self.ui);
        forgotten
    }

    /// Adopt a window the endpoint named, on the same terms as the actor that
    /// announced it.
    ///
    /// The judgement and the write happen in the shared cell — the copy every
    /// request is built from — and the UI's own copy is then read back from it
    /// whatever the answer, the road included: the mark the meter paints is a
    /// fact about the number in force, so it cannot lag it. Judging against the
    /// frame-cached copy instead was a strand: an actor's handle can have
    /// learned a window the UI has not heard about yet, and the UI then refused
    /// a number that was already in force (finding D21).
    ///
    /// Returns whether anything changed. A window the human stated is never
    /// touched, and an implausible complaint is refused — by the shared cell as
    /// well, so the two sides cannot disagree about a number.
    pub fn learn_context(&mut self, tokens: usize, source: WindowSource) -> bool {
        // The clamp, the refusal of a stated window and "nothing changed" are
        // all `adopt_context`'s; the trust policy is `learn_window`'s. A
        // poisoned cell is another thread's panic: the UI keeps its own copy,
        // so the answer is the same "not taken" the actor would get.
        let learned = self.shared.learn_window(tokens, source).unwrap_or(false);
        // Read the resulting window back into the UI's copy, whatever the
        // answer: the shared cell is the copy in force, so this is what makes
        // "the UI shows a window the actor does not have" unreachable by a
        // decision of either side.
        if let Ok(cfg) = self.shared.config() {
            self.ui.context_tokens = cfg.context_tokens;
            self.ui.context_source = cfg.context_source;
        }
        learned
    }

    /// Point the cell at another tree's cell, keeping the configuration the UI
    /// shows.
    ///
    /// Ctrl-N starts a fresh root, whose cell is what a later `/model` has to
    /// reach or the new actor never hears about it. The configuration in force
    /// travels with the swap: the new tree starts where the old one left off
    /// rather than on the values the process started with.
    pub fn adopt_handle(&mut self, shared: ConfigHandle) {
        self.shared = shared;
        self.shared.adopt(&self.ui);
    }
}

/// What an agent actor is given: the tree's configuration, readable, and — for
/// the one fact an endpoint can teach — writable through one path.
///
/// Deliberately narrower than the cell it points at: no setter, no raw lock.
/// An actor that wanted to change the endpoint is not a thing mush has, and one
/// that learns the window must go through [`ConfigHandle::learn_context`], which
/// takes the announcement as its argument and calls it exactly when the number
/// landed (see `AgentCtx::learn_context` in `agent.rs`) so the UI hears about it
/// instead of a mutex write nobody watches.
#[derive(Clone)]
pub struct ConfigHandle {
    shared: Arc<Mutex<Config>>,
}

impl ConfigHandle {
    /// A cell of its own, for a tree that is starting.
    pub fn own(cfg: Config) -> Self {
        Self {
            shared: Arc::new(Mutex::new(cfg)),
        }
    }

    /// The configuration to send a request with. A snapshot, not a borrow: the
    /// lock is held for as long as it takes to copy a handful of strings, never
    /// across the request itself.
    pub fn config(&self) -> Result<Config, String> {
        self.shared
            .lock()
            .map(|cfg| cfg.clone())
            .map_err(|_| POISONED.to_string())
    }

    /// Adopt a window the endpoint named: the width this tree measures its next
    /// request against, and — announced by `announce` — the number the UI
    /// shows.
    ///
    /// `announce` is called exactly when the number landed, and it is an
    /// argument rather than a convention because the raw write below is
    /// private: a caller that learns a window and does not tell the UI is the
    /// state finding D21 stranded the screen in. The one caller,
    /// `AgentCtx::learn_context`, passes the event itself.
    ///
    /// `Ok(false)` is a number that was not worth taking: the human stated a
    /// window, or a complaint looked implausible. A caller reads that as "the
    /// endpoint said nothing usable", not as a failure.
    pub fn learn_context(
        &self,
        tokens: usize,
        source: WindowSource,
        announce: impl FnOnce(),
    ) -> Result<bool, String> {
        let learned = self.learn_window(tokens, source)?;
        if learned {
            announce();
        }
        Ok(learned)
    }

    /// The one raw write of a window into the shared cell, private on purpose:
    /// every actor's road to the cell goes through [`Self::learn_context`],
    /// which is what makes the announcement impossible to forget (finding
    /// D21).
    fn learn_window(&self, tokens: usize, source: WindowSource) -> Result<bool, String> {
        let mut shared = self.shared.lock().map_err(|_| POISONED.to_string())?;
        if !believable(shared.context_tokens, tokens, source) {
            return Ok(false);
        }
        Ok(shared.adopt_context(tokens, source))
    }

    /// Copy a whole configuration in. Private on purpose: the only writer of a
    /// *human's* edit is `ConfigCell::edit`, and an actor has no business
    /// inventing one.
    fn adopt(&self, cfg: &Config) {
        // A poisoned lock is another thread's panic. The UI keeps its own copy
        // of the configuration, so the worst case is that one edit does not
        // reach the actors rather than a broken tree.
        if let Ok(mut shared) = self.shared.lock() {
            *shared = cfg.clone();
        }
    }

    /// Whether two handles are the same cell. Ctrl-N starts a fresh tree, and
    /// what makes the UI adopt its handle is that a later `/model` has to reach
    /// *that* actor: the test for it asks this, rather than reaching for the
    /// lock the handle deliberately does not expose.
    #[cfg(test)]
    pub fn same_cell(&self, other: &ConfigHandle) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

#[cfg(test)]
mod tests {
    use super::{believable, ConfigCell, ConfigHandle, WindowSource};
    use mush_core::Config;

    /// The handle's call the way `AgentCtx` makes it, minus the event: these
    /// tests have no tree to tell, and `learn_context` takes the announcement
    /// as the caller's.
    fn learn(handle: &ConfigHandle, tokens: usize, source: WindowSource) -> Result<bool, String> {
        handle.learn_context(tokens, source, || {})
    }

    fn cell() -> ConfigCell {
        let mut cfg = Config::new("http://127.0.0.1:1", "a-model", None);
        // A window as an endpoint's model list would have stated it.
        cfg.context_tokens = 128_000;
        ConfigCell::own(cfg)
    }

    /// The whole point of the module: the UI's copy and the actors' copy are
    /// one configuration, so an edit cannot reach only one of them.
    #[test]
    fn an_edit_reaches_a_handle_taken_before_it() {
        let mut cell = cell();
        // Taken first, the way `App::new` takes the root's handle and the
        // actors of the tree hold it for their whole life.
        let handle = cell.handle();
        assert_eq!(handle.config().unwrap().model, "a-model");

        cell.edit(|cfg| cfg.set_model("another-model"));

        assert_eq!(cell.ui().model, "another-model", "the UI reads the edit");
        assert_eq!(
            handle.config().unwrap().model,
            "another-model",
            "and so does every actor holding a handle from before it"
        );
    }

    /// A window an actor learned from the endpoint has to be the window the UI
    /// shows: one path writes both.
    #[test]
    fn a_learned_window_reaches_both_sides() {
        let mut cell = cell();
        assert!(
            cell.learn_context(64_000, WindowSource::Advertised),
            "a window the model list states is taken"
        );
        assert_eq!(cell.ui().context_tokens, 64_000);
        assert_eq!(cell.handle().config().unwrap().context_tokens, 64_000);
        assert_eq!(cell.ui().context_source, WindowSource::Advertised);
        assert_eq!(
            cell.handle().config().unwrap().context_source,
            WindowSource::Advertised,
            "the road travels with the number, on both sides"
        );
        assert!(
            !cell.learn_context(64_000, WindowSource::Advertised),
            "learning the number twice changes nothing"
        );
    }

    /// The one rule that rides in the cell, because the endpoint, the key and
    /// the host are one fact: an edit that moves the endpoint to another host
    /// forgets the key — in both copies — unless the same write states a new
    /// one (findings C6, D6). With every endpoint writer behind this function,
    /// no request can be built with a key whose endpoint changed under it.
    #[test]
    fn a_host_change_forgets_the_key_in_both_copies() {
        let mut cell = ConfigCell::own(Config::new("http://old:1", "m", Some("sk-old".into())));
        let handle = cell.handle();
        assert!(
            cell.edit(|cfg| cfg.set_base_url("http://new:2")),
            "the host changed and a key was dropped"
        );
        assert_eq!(cell.ui().api_key, None, "the UI's copy");
        assert_eq!(handle.config().unwrap().api_key, None, "the actors' copy");

        // A path on the same host is the same destination: the key stays.
        let mut cell = ConfigCell::own(Config::new("http://old:1", "m", Some("sk-old".into())));
        assert!(!cell.edit(|cfg| cfg.set_base_url("http://old:1/proxy")));
        assert_eq!(cell.ui().api_key.as_deref(), Some("sk-old"));

        // A key stated in the same write is for the new endpoint: it is the
        // writer's decision, not a leftover of the old one.
        let mut cell = ConfigCell::own(Config::new("http://old:1", "m", Some("sk-old".into())));
        assert!(!cell.edit(|cfg| {
            cfg.set_base_url("http://new:2");
            cfg.api_key = Some("sk-new".into());
        }));
        assert_eq!(cell.ui().api_key.as_deref(), Some("sk-new"));
    }

    /// The window the human stated is theirs: no endpoint, however plausible,
    /// talks mush out of it.
    #[test]
    fn a_stated_window_is_never_learned_over() {
        let mut cell = cell();
        cell.edit(|cfg| cfg.set_context(32_768));

        assert!(!cell.learn_context(64_000, WindowSource::Advertised));
        assert!(!cell.learn_context(16_000, WindowSource::Complaint));
        assert_eq!(cell.ui().context_tokens, 32_768);
        assert_eq!(cell.handle().config().unwrap().context_tokens, 32_768);
        assert_eq!(cell.ui().context_source, WindowSource::Stated);
    }

    /// The trust policy is one function, and it is the shape a complaint is
    /// held to: a smaller window is taken, a collapse is not (finding A3).
    #[test]
    fn a_complaint_has_to_look_plausible() {
        assert!(believable(128_000, 64_000, WindowSource::Complaint));
        assert!(believable(128_000, 16_000, WindowSource::Complaint));
        assert!(
            !believable(128_000, 10, WindowSource::Complaint),
            "a rate-limit body must not teach mush a ten-token window"
        );
        assert!(
            !believable(128_000, 200_000, WindowSource::Complaint),
            "a complaint that would grow the window is not what a complaint says"
        );
        assert!(
            believable(128_000, 10, WindowSource::Advertised),
            "a model list is a field, not prose: it is taken as stated (and clamped)"
        );
        assert!(
            !believable(128_000, 200_000, WindowSource::Stated),
            "the human's own number is not a learnable one"
        );
        assert!(
            !believable(128_000, 200_000, WindowSource::Table),
            "mush's own assumption is not a fact an endpoint taught"
        );
    }

    /// The handle an actor is given writes a window and nothing else; a
    /// complaint it does not believe leaves the cell exactly as it was.
    #[test]
    fn a_handle_learns_only_what_it_believes() {
        // A tree's cell as an endpoint's model list would have left it: 128k,
        // learned and not stated.
        let handle = ConfigHandle::own(Config::new("http://127.0.0.1:1", "m", None));
        assert!(learn(&handle, 128_000, WindowSource::Advertised).expect("an unpoisoned cell"));

        assert!(
            !learn(&handle, 10, WindowSource::Complaint).expect("an unpoisoned cell"),
            "an implausible complaint is refused"
        );
        assert_eq!(handle.config().unwrap().context_tokens, 128_000);
        assert!(
            learn(&handle, 32_000, WindowSource::Complaint).expect("an unpoisoned cell"),
            "a plausible one is taken"
        );
        assert_eq!(handle.config().unwrap().context_tokens, 32_000);
    }

    /// Finding D21: the two copies cannot be left apart by a learn decision of
    /// either side.
    ///
    /// The audit's probe built this state: the shared cell had learned 16_000
    /// through a handle (an actor adopting a complaint) while the UI's copy
    /// still showed 128_000, and the UI's next learn was judged against its own
    /// stale copy — where a 4_000 complaint is implausible, 32× smaller than
    /// 128k — and refused, leaving the bar and the tool caps reading one window
    /// while every request used another. After the fix the judgement happens
    /// against the copy in force, and whatever the answer the UI's copy is read
    /// back from it.
    #[test]
    fn the_two_copies_agree_or_the_number_is_refused() {
        let mut cell = cell();
        let handle = cell.handle();
        assert_eq!(cell.ui().context_tokens, 128_000, "the UI's own copy");

        // An actor's handle learns first, the way a complaint on a request
        // does; the UI has not heard about it yet.
        assert!(learn(&handle, 16_000, WindowSource::Complaint).unwrap());
        assert_eq!(handle.config().unwrap().context_tokens, 16_000);
        assert_eq!(cell.ui().context_tokens, 128_000, "the stale UI copy");

        // The number the probe's UI was refused: plausible against the 16k in
        // force, implausible against the UI's stale 128k.
        assert!(cell.learn_context(4_000, WindowSource::Complaint));
        assert_eq!(cell.ui().context_tokens, 4_000, "the UI adopted it");
        assert_eq!(
            handle.config().unwrap().context_tokens,
            4_000,
            "and the actors read the same number"
        );

        // A number the cell in force refuses — a complaint that would grow the
        // window — leaves both copies on the window in force.
        assert!(!cell.learn_context(200_000, WindowSource::Complaint));
        assert_eq!(cell.ui().context_tokens, 4_000);
        assert_eq!(handle.config().unwrap().context_tokens, 4_000);
    }

    /// Ctrl-N replaces the tree: the fresh root gets the configuration in
    /// force, and a later edit reaches it.
    #[test]
    fn a_new_trees_cell_starts_where_the_old_one_left_off() {
        let mut cell = cell();
        cell.edit(|cfg| cfg.set_model("mine"));

        let fresh = ConfigHandle::own(Config::new("http://127.0.0.1:1", "stale", None));
        cell.adopt_handle(fresh.clone());

        assert_eq!(
            fresh.config().unwrap().model,
            "mine",
            "it starts where we are"
        );
        cell.edit(|cfg| cfg.set_model("after"));
        assert_eq!(
            fresh.config().unwrap().model,
            "after",
            "and the new tree hears the next edit"
        );
    }
}

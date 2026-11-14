//! Chord remapping on the sending side.
//!
//! A rule maps a set of held keys (`from`) to a key sequence (`to`) that is
//! synthesized on the *remote* peer.
//!
//! Nothing is ever delayed, so ordinary typing keeps its latency. Instead the
//! remapper tracks which modifiers the peer currently believes are held. When a
//! chord completes it releases exactly those on the peer, then taps the rule's
//! `to` sequence, so the peer never observes the chord's modifiers on the
//! synthesized key. If the user keeps holding the modifiers afterwards, they
//! are re-announced on the next unrelated key press.
//!
//! A chord must be pressed modifiers-first (`Shift+Alt+S`): the trigger key is
//! what completes it. Chords consisting solely of non-modifier keys are not
//! supported, because detecting those requires delaying key delivery.

use std::collections::HashSet;

use input_event::{Event, KeyboardEvent, scancode};

/// A single chord rule, as configured by the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rule {
    pub from: Vec<u32>,
    pub to: Vec<u32>,
}

struct CompiledRule {
    from: HashSet<u32>,
    to: Vec<u32>,
}

pub(crate) struct Remap {
    rules: Vec<CompiledRule>,
    /// modifiers physically held locally, in press order
    held: Vec<u32>,
    /// modifiers the peer currently believes are held
    peer_down: Vec<u32>,
    /// trigger key of a chord that already fired; its auto-repeat and release
    /// are swallowed
    fired: Option<u32>,
}

impl Remap {
    pub(crate) fn new(rules: Vec<Rule>) -> Self {
        Self {
            rules: rules
                .into_iter()
                .filter(|r| !r.from.is_empty() && !r.to.is_empty())
                .map(|r| CompiledRule {
                    from: r.from.into_iter().collect(),
                    to: r.to,
                })
                .collect(),
            held: Vec::new(),
            peer_down: Vec::new(),
            fired: None,
        }
    }

    pub(crate) fn set_rules(&mut self, rules: Vec<Rule>) {
        log::info!("applying {} chord remap rule(s)", rules.len());
        *self = Self::new(rules);
    }

    /// Translate one captured event into the events to forward to the peer.
    /// An empty result means the event is swallowed.
    pub(crate) fn apply(&mut self, event: Event) -> Vec<Event> {
        match event {
            Event::Pointer(_) => vec![event],
            Event::Keyboard(KeyboardEvent::Key { time, key, state }) => {
                if state == 1 {
                    self.key_down(time, key)
                } else {
                    self.key_up(time, key)
                }
            }
            // Modifier state events are forwarded untouched: key events already
            // maintain the peer's modifier mask, and rewriting the locked bits
            // here would clobber a CapsLock the user has on.
            Event::Keyboard(KeyboardEvent::Modifiers { .. }) => vec![event],
        }
    }

    fn key_down(&mut self, time: u32, key: u32) -> Vec<Event> {
        if self.fired == Some(key) {
            // auto-repeat of a chord trigger: the tap already happened
            return Vec::new();
        }

        if is_modifier(key) {
            if !self.held.contains(&key) {
                self.held.push(key);
            }
            if !self.peer_down.contains(&key) {
                self.peer_down.push(key);
            }
            return vec![key_event(time, key, 1)];
        }

        let mut candidate: HashSet<u32> = self.held.iter().copied().collect();
        candidate.insert(key);

        if let Some(rule) = self.rules.iter().find(|r| r.from == candidate) {
            let to = rule.to.clone();
            self.fired = Some(key);
            let mut out = self.release_peer_mods();
            out.extend(tap(&to));
            return out;
        }

        let mut out = Vec::new();
        // A previous chord force-released the modifiers the user is still
        // holding; announce them again so this key is modified as expected.
        if self.peer_down.is_empty() && !self.held.is_empty() {
            for &key in &self.held {
                out.push(key_event(0, key, 1));
            }
            self.peer_down.clone_from(&self.held);
        }
        out.push(key_event(time, key, 1));
        out
    }

    fn key_up(&mut self, time: u32, key: u32) -> Vec<Event> {
        if self.fired == Some(key) {
            self.fired = None;
            return Vec::new();
        }
        if is_modifier(key) {
            self.held.retain(|&k| k != key);
            if let Some(i) = self.peer_down.iter().position(|&k| k == key) {
                self.peer_down.remove(i);
                return vec![key_event(time, key, 0)];
            }
            // already force-released when the chord fired
            return Vec::new();
        }
        vec![key_event(time, key, 0)]
    }

    /// Forget all tracked state. Called when the capture is released: nothing
    /// is withheld, so this only prevents a stale chord from matching later.
    pub(crate) fn reset(&mut self) {
        self.held.clear();
        self.peer_down.clear();
        self.fired = None;
    }

    /// Release the modifiers the peer believes are held.
    fn release_peer_mods(&mut self) -> Vec<Event> {
        let out = self
            .peer_down
            .iter()
            .map(|&key| key_event(0, key, 0))
            .collect();
        self.peer_down.clear();
        out
    }
}

fn key_event(time: u32, key: u32, state: u8) -> Event {
    Event::Keyboard(KeyboardEvent::Key { time, key, state })
}

fn is_modifier(key: u32) -> bool {
    matches!(
        scancode::Linux::try_from(key),
        Ok(scancode::Linux::KeyLeftShift
            | scancode::Linux::KeyRightShift
            | scancode::Linux::KeyLeftCtrl
            | scancode::Linux::KeyRightCtrl
            | scancode::Linux::KeyLeftAlt
            | scancode::Linux::KeyRightalt
            | scancode::Linux::KeyLeftMeta
            | scancode::Linux::KeyRightmeta)
    )
}

/// press every key in `keys`, then release them in reverse order
fn tap(keys: &[u32]) -> Vec<Event> {
    let mut out: Vec<Event> = keys.iter().map(|&key| key_event(0, key, 1)).collect();
    out.extend(keys.iter().rev().map(|&key| key_event(0, key, 0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::scancode::Linux::*;

    fn key(key: scancode::Linux, state: u8) -> Event {
        key_event(0, key as u32, state)
    }

    fn sas_to_s() -> Remap {
        Remap::new(vec![Rule {
            from: vec![KeyLeftShift as u32, KeyLeftAlt as u32, KeyS as u32],
            to: vec![KeyS as u32],
        }])
    }

    /// feed a sequence of events, collecting everything forwarded
    fn run(remap: &mut Remap, events: Vec<Event>) -> Vec<Event> {
        events.into_iter().flat_map(|e| remap.apply(e)).collect()
    }

    #[test]
    fn chord_releases_modifiers_then_taps_target() {
        let mut remap = sas_to_s();
        let out = run(
            &mut remap,
            vec![key(KeyLeftShift, 1), key(KeyLeftAlt, 1), key(KeyS, 1)],
        );
        assert_eq!(
            out,
            vec![
                key(KeyLeftShift, 1),
                key(KeyLeftAlt, 1),
                // chord fires: the peer's modifiers go up, then the tap
                key(KeyLeftShift, 0),
                key(KeyLeftAlt, 0),
                key(KeyS, 1),
                key(KeyS, 0),
            ]
        );
    }

    #[test]
    fn chord_trigger_release_and_repeat_are_swallowed() {
        let mut remap = sas_to_s();
        run(
            &mut remap,
            vec![key(KeyLeftShift, 1), key(KeyLeftAlt, 1), key(KeyS, 1)],
        );
        // auto-repeat while held
        assert_eq!(run(&mut remap, vec![key(KeyS, 1)]), vec![]);
        // releasing the trigger emits nothing (the tap already did)
        assert_eq!(run(&mut remap, vec![key(KeyS, 0)]), vec![]);
        // the modifiers were force-released, so their physical release is not
        // forwarded a second time
        assert_eq!(
            run(&mut remap, vec![key(KeyLeftAlt, 0), key(KeyLeftShift, 0)]),
            vec![]
        );
    }

    #[test]
    fn held_modifiers_are_reannounced_after_chord() {
        let mut remap = sas_to_s();
        run(
            &mut remap,
            vec![key(KeyLeftShift, 1), key(KeyLeftAlt, 1), key(KeyS, 1)],
        );
        // Shift+Alt still physically held: the next key must still be modified
        assert_eq!(
            run(&mut remap, vec![key(KeyA, 1)]),
            vec![
                key(KeyLeftShift, 1),
                key(KeyLeftAlt, 1),
                key(KeyA, 1)
            ]
        );
        // ...and its release passes through normally
        assert_eq!(run(&mut remap, vec![key(KeyA, 0)]), vec![key(KeyA, 0)]);
        assert_eq!(
            run(&mut remap, vec![key(KeyLeftShift, 0)]),
            vec![key(KeyLeftShift, 0)]
        );
    }

    #[test]
    fn chord_fires_again_on_second_press() {
        let mut remap = sas_to_s();
        let first = run(
            &mut remap,
            vec![key(KeyLeftShift, 1), key(KeyLeftAlt, 1), key(KeyS, 1)],
        );
        run(&mut remap, vec![key(KeyS, 0)]);
        let second = run(&mut remap, vec![key(KeyS, 1)]);
        assert_eq!(
            &first[first.len() - 2..],
            &[key(KeyS, 1), key(KeyS, 0)],
            "first press taps S"
        );
        assert_eq!(second, vec![key(KeyS, 1), key(KeyS, 0)], "second press too");
    }

    #[test]
    fn plain_shifted_key_is_untouched() {
        let mut remap = sas_to_s();
        let events = vec![
            key(KeyLeftShift, 1),
            key(KeyS, 1),
            key(KeyS, 0),
            key(KeyLeftShift, 0),
        ];
        assert_eq!(run(&mut remap, events.clone()), events);
    }

    #[test]
    fn extra_modifier_prevents_chord_match() {
        let mut remap = sas_to_s();
        // Ctrl also held: not the configured chord, so everything passes through
        let events = vec![
            key(KeyLeftShift, 1),
            key(KeyLeftAlt, 1),
            key(KeyLeftCtrl, 1),
            key(KeyS, 1),
        ];
        assert_eq!(run(&mut remap, events.clone()), events);
    }

    #[test]
    fn chord_may_emit_modifier_combination() {
        let mut remap = Remap::new(vec![Rule {
            from: vec![KeyLeftShift as u32, KeyLeftAlt as u32, KeyS as u32],
            to: vec![KeyLeftMeta as u32, KeyS as u32],
        }]);
        let out = run(
            &mut remap,
            vec![key(KeyLeftShift, 1), key(KeyLeftAlt, 1), key(KeyS, 1)],
        );
        assert_eq!(
            &out[out.len() - 4..],
            &[
                key(KeyLeftMeta, 1),
                key(KeyS, 1),
                key(KeyS, 0),
                key(KeyLeftMeta, 0)
            ]
        );
    }

    #[test]
    fn modifier_state_events_pass_through() {
        let mut remap = sas_to_s();
        let ev = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: 1,
            latched: 0,
            locked: 2,
            group: 0,
        });
        assert_eq!(run(&mut remap, vec![ev]), vec![ev]);
    }

    #[test]
    fn pointer_events_pass_through() {
        let mut remap = sas_to_s();
        let motion = Event::Pointer(input_event::PointerEvent::Motion {
            time: 0,
            dx: 1.0,
            dy: 2.0,
        });
        assert_eq!(run(&mut remap, vec![motion]), vec![motion]);
    }

    #[test]
    fn empty_remap_is_transparent() {
        let mut remap = Remap::new(vec![]);
        let events = vec![key(KeyLeftShift, 1), key(KeyS, 1), key(KeyS, 0)];
        assert_eq!(run(&mut remap, events.clone()), events);
    }

    #[test]
    fn reset_prevents_stale_chord_match() {
        let mut remap = sas_to_s();
        run(&mut remap, vec![key(KeyLeftShift, 1), key(KeyLeftAlt, 1)]);
        remap.reset();
        // after a release/re-enter, S alone must not fire the chord
        assert_eq!(run(&mut remap, vec![key(KeyS, 1)]), vec![key(KeyS, 1)]);
    }
}

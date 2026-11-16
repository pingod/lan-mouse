//! Receiver-side smoothing for mouse-wheel input.
//!
//! A wheel delivers whole notches: the capture side forwards one event per
//! detent and the receiving application jumps by a fixed amount. Trackpads look
//! smoother because their hardware already sends a stream of small deltas. This
//! module turns each notch into such a stream: the notch is queued and emitted
//! as continuous axis deltas over a short animation, shaped by an exponential
//! ease-out so the first tick still reacts immediately and the tail settles.
//!
//! The animation lives here instead of in the backends because it is the same
//! for every backend that can inject continuous scroll, and because it needs a
//! clock that the backend's `consume` call does not have.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::EmulationHandle;

/// One animation step. 125 Hz is above the refresh rate of every desktop this
/// runs on, so the receiving compositor never has to interpolate two ticks.
pub(crate) const TICK_INTERVAL: Duration = Duration::from_millis(8);

/// Below this many native units the rest is emitted in one go; the visible
/// jump is far below one pixel.
const SNAP: f64 = 0.01;

/// How much of a discrete wheel event the receiver spreads over time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrollSmoothing {
    duration: Duration,
}

impl ScrollSmoothing {
    /// No animation: notches are injected as discrete events.
    pub const OFF: Self = Self {
        duration: Duration::ZERO,
    };

    /// Clamp: below 20 ms the steps are invisible, above a second the wheel
    /// feels detached from the hand.
    pub fn from_millis(milliseconds: u64) -> Self {
        Self {
            duration: Duration::from_millis(milliseconds.clamp(20, 1000)),
        }
    }

    pub fn duration(self) -> Duration {
        self.duration
    }

    pub fn is_on(self) -> bool {
        !self.duration.is_zero()
    }
}

/// Per-handle, per-axis animation state.
pub(crate) struct Smoother {
    duration: Duration,
    /// native units one wheel notch is worth in this backend
    units_per_notch: f64,
    handles: HashMap<EmulationHandle, [AxisState; 2]>,
}

#[derive(Clone, Copy, Default)]
struct AxisState {
    /// native units still to emit; the sign is the scroll direction
    remaining: f64,
    /// when the newest input arrived, so a burst restarts the settle time
    started: Option<Instant>,
    /// when the previous tick ran
    last: Option<Instant>,
    /// when the next tick is due
    next: Option<Instant>,
}

impl Smoother {
    pub(crate) fn new(duration: Duration, units_per_notch: f64) -> Self {
        Self {
            duration,
            units_per_notch,
            handles: HashMap::new(),
        }
    }

    /// Queue `value120` (120ths of a notch) for `axis` on `handle`.
    pub(crate) fn add_notches(
        &mut self,
        handle: EmulationHandle,
        axis: u8,
        value120: i32,
        now: Instant,
    ) {
        if value120 == 0 {
            return;
        }
        let Some(slot) = axis_slot(axis) else {
            log::warn!("smooth scroll: invalid axis {axis}");
            return;
        };
        let units = f64::from(value120) / 120.0 * self.units_per_notch;
        let state = &mut self.handles.entry(handle).or_default()[slot];

        if state.remaining != 0.0 && state.remaining.signum() != units.signum() {
            /* The user reversed direction: the tail of the old direction must
             * not keep pushing against the new one. */
            state.remaining = units;
        } else {
            state.remaining += units;
        }
        state.started = Some(now);
        state.last.get_or_insert(now);
        state.next.get_or_insert(now + TICK_INTERVAL);
    }

    /// The earliest tick that is due, if any animation still has work.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.handles
            .values()
            .flatten()
            .filter(|state| state.remaining != 0.0)
            .filter_map(|state| state.next)
            .min()
    }

    /// Emit the part of every due animation that belongs to `now`.
    pub(crate) fn tick(&mut self, now: Instant, out: &mut Vec<(EmulationHandle, u8, f64)>) {
        for (&handle, axes) in self.handles.iter_mut() {
            for (slot, state) in axes.iter_mut().enumerate() {
                if state.remaining == 0.0 {
                    *state = AxisState::default();
                    continue;
                }
                let Some(next) = state.next else {
                    continue;
                };
                if next > now {
                    continue;
                }

                let started = state.started.unwrap_or(now);
                let last = state.last.unwrap_or(now);
                let elapsed = now.saturating_duration_since(started);
                let emit = if elapsed >= self.duration {
                    state.remaining
                } else {
                    /* Exponential approach with a time constant of a quarter of
                     * the duration: ~63% after the first quarter, ~98% at the
                     * end. The remaining fraction is snapped below. */
                    let tau = self.duration.as_secs_f64() / 4.0;
                    let dt = now.saturating_duration_since(last).as_secs_f64();
                    state.remaining * (1.0 - (-dt / tau).exp())
                };

                state.remaining -= emit;
                if state.remaining.abs() < SNAP || elapsed >= self.duration {
                    state.remaining = 0.0;
                }

                if emit != 0.0 {
                    out.push((handle, slot as u8, emit));
                }

                if state.remaining == 0.0 {
                    *state = AxisState::default();
                } else {
                    state.last = Some(now);
                    state.next = Some(now + TICK_INTERVAL);
                }
            }
        }
    }

    /// Emit everything still queued for `handle` and forget it.
    ///
    /// Used when an event that must not overtake a scroll arrives (a click, a
    /// key) and when the peer leaves.
    pub(crate) fn flush(
        &mut self,
        handle: EmulationHandle,
        out: &mut Vec<(EmulationHandle, u8, f64)>,
    ) {
        let Some(axes) = self.handles.remove(&handle) else {
            return;
        };
        for (slot, state) in axes.iter().enumerate() {
            if state.remaining != 0.0 {
                out.push((handle, slot as u8, state.remaining));
            }
        }
    }

    pub(crate) fn remove(&mut self, handle: EmulationHandle) {
        self.handles.remove(&handle);
    }

    pub(crate) fn clear(&mut self) {
        self.handles.clear();
    }
}

fn axis_slot(axis: u8) -> Option<usize> {
    match axis {
        0 | 1 => Some(axis as usize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(
        smoother: &mut Smoother,
        now: &mut Instant,
        out: &mut Vec<(EmulationHandle, u8, f64)>,
        steps: usize,
    ) -> f64 {
        let mut total = 0.0;
        for _ in 0..steps {
            *now += TICK_INTERVAL;
            out.clear();
            smoother.tick(*now, out);
            total += out.iter().map(|(_, _, value)| *value).sum::<f64>();
            if smoother.deadline().is_none() {
                break;
            }
        }
        total
    }

    #[test]
    fn a_notch_is_spread_over_several_ticks_and_reaches_the_target() {
        let mut smoother = Smoother::new(Duration::from_millis(120), 15.0);
        let start = Instant::now();
        let mut now = start;
        let mut out = Vec::new();

        smoother.add_notches(1, 0, 120, start);
        assert_eq!(smoother.deadline(), Some(start + TICK_INTERVAL));

        now += TICK_INTERVAL;
        smoother.tick(now, &mut out);
        assert_eq!(out.len(), 1, "one axis tick at a time");
        assert_eq!(out[0].0, 1);
        assert_eq!(out[0].1, 0);
        assert!(
            out[0].2 > 0.0 && out[0].2 < 15.0,
            "the first tick is a fraction: {}",
            out[0].2
        );

        let mut total = out[0].2;
        total += drive(&mut smoother, &mut now, &mut out, 100);
        assert!((total - 15.0).abs() < 1e-9, "emitted {total}");
        assert!(smoother.deadline().is_none());
    }

    #[test]
    fn a_reversed_wheel_replaces_the_tail_instead_of_fighting_it() {
        let mut smoother = Smoother::new(Duration::from_millis(120), 15.0);
        let start = Instant::now();
        let mut now = start;
        let mut out = Vec::new();

        smoother.add_notches(1, 0, 120, start);
        now += TICK_INTERVAL;
        smoother.tick(now, &mut out);
        let forward = out.iter().map(|(_, _, v)| *v).sum::<f64>();
        assert!(forward > 0.0);

        smoother.add_notches(1, 0, -120, now);
        out.clear();
        let mut backwards = 0.0;
        for _ in 0..100 {
            now += TICK_INTERVAL;
            out.clear();
            smoother.tick(now, &mut out);
            backwards += out.iter().map(|(_, _, v)| *v).sum::<f64>();
            if smoother.deadline().is_none() {
                break;
            }
        }

        assert!(
            (backwards + 15.0).abs() < 1e-9,
            "the reverse emitted {backwards}, not the full notch"
        );
    }

    #[test]
    fn flushing_emits_the_rest_once() {
        let mut smoother = Smoother::new(Duration::from_millis(120), 15.0);
        let now = Instant::now();
        let mut out = Vec::new();

        smoother.add_notches(7, 1, 60, now);
        smoother.flush(7, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 7);
        assert_eq!(out[0].1, 1);
        assert!((out[0].2 - 7.5).abs() < 1e-9, "half a notch is 7.5 units");

        out.clear();
        smoother.flush(7, &mut out);
        assert!(out.is_empty());
        assert!(smoother.deadline().is_none());
    }

    #[test]
    fn axes_and_handles_animate_independently() {
        let mut smoother = Smoother::new(Duration::from_millis(120), 15.0);
        let start = Instant::now();
        let mut now = start;
        let mut out = Vec::new();

        smoother.add_notches(1, 0, 120, start);
        smoother.add_notches(1, 1, 240, start);
        smoother.add_notches(2, 0, -120, start + TICK_INTERVAL);

        assert_eq!(smoother.deadline(), Some(start + TICK_INTERVAL));
        now += TICK_INTERVAL;
        smoother.tick(now, &mut out);
        assert_eq!(out.len(), 2, "two axes are due for handle 1");
        assert!(out.iter().all(|(handle, _, _)| *handle == 1));

        out.clear();
        now += TICK_INTERVAL;
        smoother.tick(now, &mut out);
        assert!(
            out.iter()
                .any(|(handle, axis, _)| *handle == 2 && *axis == 0),
            "handle 2 is due on its own schedule"
        );
    }

    #[test]
    fn nothing_is_emitted_before_the_tick_is_due() {
        let mut smoother = Smoother::new(Duration::from_millis(120), 15.0);
        let start = Instant::now();
        let mut out = Vec::new();

        smoother.add_notches(1, 0, 120, start);
        smoother.tick(start, &mut out);
        assert!(out.is_empty());
        assert_eq!(smoother.deadline(), Some(start + TICK_INTERVAL));
    }

    #[test]
    fn removing_a_handle_drops_its_animation() {
        let mut smoother = Smoother::new(Duration::from_millis(120), 15.0);
        smoother.add_notches(1, 0, 120, Instant::now());
        smoother.remove(1);
        assert!(smoother.deadline().is_none());
    }

    #[test]
    fn the_duration_is_clamped_to_a_sane_range() {
        assert_eq!(
            ScrollSmoothing::from_millis(1).duration(),
            Duration::from_millis(20)
        );
        assert_eq!(
            ScrollSmoothing::from_millis(5000).duration(),
            Duration::from_millis(1000)
        );
        assert!(!ScrollSmoothing::OFF.is_on());
    }
}

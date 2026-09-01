//! The badge's four buttons, as a state machine with no hardware in it.
//!
//! This crate deliberately has no ESP-IDF dependency, for the same reason
//! `badge-screen` does not: the firmware owns the GPIO pins and nothing else,
//! so the part that decides what a press *means* is unit testable from a
//! development host.
//!
//! The badge has four buttons and five outcomes. UP and DOWN answer on press.
//! LEFT and RIGHT answer on *release*, because holding both together is the
//! simulated-crash gesture and the combo has to get a chance to be recognised
//! before either release is read as an answer.

use std::time::{Duration, Instant};

/// How long both side buttons must be held to simulate a crash.
pub const PANIC_HOLD: Duration = Duration::from_millis(500);

/// A sample of the four buttons. `true` means pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buttons {
    pub up: bool,
    pub right: bool,
    pub down: bool,
    pub left: bool,
}

impl Buttons {
    #[must_use]
    pub fn any(self) -> bool {
        self.up || self.right || self.down || self.left
    }
}

/// Returns the two samples that make a USB HIL answer indistinguishable from
/// a complete physical button gesture to [`ButtonState`].
#[must_use]
pub const fn answer_gesture(index: u8) -> Option<[Buttons; 2]> {
    let released = Buttons {
        up: false,
        right: false,
        down: false,
        left: false,
    };
    let pressed = match index {
        0 => Buttons {
            up: true,
            ..released
        },
        1 => Buttons {
            right: true,
            ..released
        },
        2 => Buttons {
            left: true,
            ..released
        },
        3 => Buttons {
            down: true,
            ..released
        },
        _ => return None,
    };
    Some([pressed, released])
}

/// What the badge decided a gesture meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The index into the question's four answers.
    Answer(u8),
    /// Both side buttons held past [`PANIC_HOLD`]: simulate a Worker crash.
    Panic,
}

/// Which side button is waiting for its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    const fn answer(self) -> u8 {
        match self {
            Self::Left => 2,
            Self::Right => 1,
        }
    }
}

/// Where the badge is in a gesture.
///
/// This replaced four correlated booleans -- `left_armed`, `right_armed`,
/// `combo_started` and `suppress_until_release` -- whose sixteen combinations
/// described five real situations. Two of the leftovers mattered: both sides
/// armed at once was reachable and wedged the badge (see
/// `rolling_from_one_side_to_the_other_does_not_wedge`), and every branch that
/// changed one flag had to remember to clear the others by hand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonState {
    /// Nothing held, nothing pending.
    #[default]
    Idle,
    /// A side button is down. Its answer fires when it is released.
    Armed(Side),
    /// Both side buttons are down; `since` times the hold.
    Combo { since: Instant },
    /// A power-up overlay took the screen. Ignore everything until every
    /// button is up, so a press aimed at the overlay is not read as an answer
    /// to the question underneath it.
    SuppressedUntilRelease,
}

impl ButtonState {
    /// Advances one poll tick.
    ///
    /// Returns the next state and the choice this tick produced, if any. Total:
    /// there is no error case, and a caller that keeps feeding samples always
    /// gets a well-defined state back.
    #[must_use]
    pub fn advance(
        self,
        buttons: Buttons,
        powerup_active: bool,
        now: Instant,
        panic_hold: Duration,
    ) -> (Self, Option<Choice>) {
        // The overlay wins over anything in progress, and takes the gesture
        // with it: whatever was armed was aimed at the previous screen.
        if powerup_active {
            return (Self::SuppressedUntilRelease, None);
        }
        if self == Self::SuppressedUntilRelease {
            let next = if buttons.any() {
                Self::SuppressedUntilRelease
            } else {
                Self::Idle
            };
            return (next, None);
        }

        if buttons.left && buttons.right {
            let since = match self {
                Self::Combo { since } => since,
                _ => now,
            };
            if now.duration_since(since) >= panic_hold {
                return (Self::Idle, Some(Choice::Panic));
            }
            return (Self::Combo { since }, None);
        }

        // Not a combo this tick. A combo that just broke disarms both sides,
        // so letting go of them does not also answer the question.
        let armed = match self {
            Self::Armed(side) => Some(side),
            _ => None,
        };

        if buttons.up {
            return (Self::Idle, Some(Choice::Answer(0)));
        }
        if buttons.down {
            return (Self::Idle, Some(Choice::Answer(3)));
        }
        // A side that is down now supersedes one that was armed before. At a
        // 20 ms sample a roll from one side to the other lands in a single
        // tick, and the button still under the thumb is the one that meant it.
        if buttons.left {
            return (Self::Armed(Side::Left), None);
        }
        if buttons.right {
            return (Self::Armed(Side::Right), None);
        }
        match armed {
            Some(side) => (Self::Idle, Some(Choice::Answer(side.answer()))),
            None => (Self::Idle, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a sequence of samples through the machine and collects whatever
    /// choices come out, so a test reads as the gesture it describes.
    fn run(samples: &[(Buttons, bool)]) -> (ButtonState, Vec<Choice>) {
        let start = Instant::now();
        let mut state = ButtonState::default();
        let mut choices = Vec::new();
        for (tick, (buttons, powerup)) in samples.iter().enumerate() {
            let now = start + Duration::from_millis(20 * tick as u64);
            let (next, choice) = state.advance(*buttons, *powerup, now, PANIC_HOLD);
            state = next;
            choices.extend(choice);
        }
        (state, choices)
    }

    const NONE: Buttons = Buttons {
        up: false,
        right: false,
        down: false,
        left: false,
    };
    const UP: Buttons = Buttons { up: true, ..NONE };
    const DOWN: Buttons = Buttons { down: true, ..NONE };
    const LEFT: Buttons = Buttons { left: true, ..NONE };
    const RIGHT: Buttons = Buttons {
        right: true,
        ..NONE
    };
    const BOTH: Buttons = Buttons {
        left: true,
        right: true,
        ..NONE
    };

    #[test]
    fn up_and_down_answer_on_press() {
        assert_eq!(run(&[(UP, false)]).1, [Choice::Answer(0)]);
        assert_eq!(run(&[(DOWN, false)]).1, [Choice::Answer(3)]);
    }

    #[test]
    fn every_hil_gesture_uses_the_real_button_mapping() {
        for expected_index in 0..=3 {
            let samples = answer_gesture(expected_index)
                .expect("all four answer indexes have a gesture")
                .map(|buttons| (buttons, false));
            assert_eq!(run(&samples).1, [Choice::Answer(expected_index)]);
        }
        assert_eq!(answer_gesture(4), None);
    }

    #[test]
    fn the_sides_answer_on_release_not_on_press() {
        assert_eq!(run(&[(LEFT, false)]).1, [], "still held, still silent");
        assert_eq!(run(&[(LEFT, false), (NONE, false)]).1, [Choice::Answer(2)]);
        assert_eq!(run(&[(RIGHT, false), (NONE, false)]).1, [Choice::Answer(1)]);
    }

    #[test]
    fn holding_both_sides_past_the_threshold_panics() {
        // PANIC_HOLD is 500 ms and the tick is 20 ms, so 26 ticks clears it.
        let samples: Vec<_> = std::iter::repeat_n((BOTH, false), 26).collect();
        assert_eq!(run(&samples).1, [Choice::Panic]);
    }

    #[test]
    fn a_combo_released_early_answers_nothing() {
        // The whole reason the sides answer on release: an aborted crash
        // gesture must not also count as picking an answer.
        let (state, choices) = run(&[(BOTH, false), (BOTH, false), (NONE, false)]);
        assert_eq!(choices, []);
        assert_eq!(state, ButtonState::Idle);
    }

    #[test]
    fn releasing_one_side_of_a_combo_arms_the_other() {
        // Matches the behaviour of the four-boolean version it replaced.
        let (_, choices) = run(&[(BOTH, false), (RIGHT, false), (NONE, false)]);
        assert_eq!(choices, [Choice::Answer(1)]);
    }

    #[test]
    fn rolling_from_one_side_to_the_other_does_not_wedge() {
        // The four-boolean version could arm both sides at once here -- right
        // stayed armed while left armed on the same tick -- and once both were
        // armed neither release could ever answer again, because each check
        // required the other side to be clear. The badge silently stopped
        // taking left and right for the rest of the question.
        let (state, choices) = run(&[(RIGHT, false), (LEFT, false), (NONE, false)]);
        assert_eq!(choices, [Choice::Answer(2)], "the side still held wins");
        assert_eq!(state, ButtonState::Idle);

        // And it still works on the next question.
        let (_, again) = run(&[
            (RIGHT, false),
            (LEFT, false),
            (NONE, false),
            (RIGHT, false),
            (NONE, false),
        ]);
        assert_eq!(again, [Choice::Answer(2), Choice::Answer(1)]);
    }

    #[test]
    fn the_wedge_was_symmetric_so_the_fix_must_be_too() {
        // An exhaustive search over the four-boolean version showed both LR
        // and RL wedging, not just RL. Whichever side is still down wins.
        let (state, choices) = run(&[(LEFT, false), (RIGHT, false), (NONE, false)]);
        assert_eq!(choices, [Choice::Answer(1)]);
        assert_eq!(state, ButtonState::Idle);
    }

    #[test]
    fn a_crossover_caught_mid_release_still_answers_once() {
        // The wedge needed the crossover to land inside a single 20 ms sample.
        // When a tick does catch the gap, the first release answers and the
        // second press starts a fresh gesture -- two choices, not zero.
        let (_, choices) = run(&[(RIGHT, false), (NONE, false), (LEFT, false), (NONE, false)]);
        assert_eq!(choices, [Choice::Answer(1), Choice::Answer(2)]);
    }

    #[test]
    fn a_powerup_swallows_the_gesture_under_it() {
        let (state, choices) = run(&[(LEFT, false), (LEFT, true), (NONE, false)]);
        assert_eq!(
            choices,
            [],
            "the press was aimed at the overlay, not the question"
        );
        assert_eq!(state, ButtonState::Idle, "released, so back in play");
    }

    #[test]
    fn suppression_lasts_until_every_button_is_up() {
        let (state, choices) = run(&[(BOTH, true), (BOTH, false), (LEFT, false)]);
        assert_eq!(choices, []);
        assert_eq!(
            state,
            ButtonState::SuppressedUntilRelease,
            "something is still down"
        );
    }

    #[test]
    fn a_powerup_cannot_be_outlasted_into_a_panic() {
        // Holding through the overlay must not accumulate hold time: the
        // combo timer restarts once the buttons come up and go down again.
        let mut samples = vec![(BOTH, true); 40];
        samples.extend(std::iter::repeat_n((BOTH, false), 10));
        assert_eq!(run(&samples).1, [], "still suppressed, never released");
    }
}

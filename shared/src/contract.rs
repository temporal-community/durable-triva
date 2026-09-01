//! The wire contract between the badges, the Workflow and the TV.
//!
//! Every type here is serialized into Temporal History, so a change is a
//! change to data that already exists: new fields need `#[serde(default)]`
//! and old payloads have to keep decoding. The `legacy_` tests below pin the
//! cases that have already happened.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de};

pub const BADGE_TASK_QUEUE: &str = "temporal-trivia-badges-v1";
// Workflow and Activity Workers share one logical Task Queue so Temporal UI's
// Workflow Workers tab can show the Mac controller and physical badges
// together. WorkerTaskTypes still prevents either process from accepting the
// other's task type.
pub const WEB_TASK_QUEUE: &str = BADGE_TASK_QUEUE;
pub const GAME_SECONDS: u64 = 60;
pub const CHAOS_DURATION_MS: u64 = 10_000;
pub const GAME_EXTENSION_MS: u64 = 30_000;
/// How frequently a badge confirms its live Activity directly with Temporal.
pub const BADGE_HEARTBEAT_INTERVAL_MS: u64 = 1_000;
/// Temporal retries a badge Activity after this long without a heartbeat.
/// Embedded Wi-Fi and Cloud RPCs can occasionally stall beyond five seconds.
pub const BADGE_HEARTBEAT_TIMEOUT_MS: u64 = 15_000;
/// A deliberate badge failure must remain silent past the Temporal timeout.
pub const BADGE_CRASH_BLACKOUT_MS: u64 = 16_000;
/// How long a badge tolerates unacknowledged Activity heartbeats before it
/// gives its question up.
///
/// Temporal's server-side timeout is the real authority. This sits below it so
/// a badge that genuinely cannot reach Cloud stops holding a question it can
/// no longer answer, while a single dropped RPC costs nothing. Failing on the
/// first error handed a healthy player's question to another badge over one
/// lost packet, and labelled it a heartbeat timeout on the TV.
pub const BADGE_HEARTBEAT_FAILURE_BUDGET_MS: u64 = 10_000;
const _: () = {
    assert!(BADGE_HEARTBEAT_INTERVAL_MS < BADGE_HEARTBEAT_TIMEOUT_MS);
    assert!(BADGE_HEARTBEAT_TIMEOUT_MS < BADGE_CRASH_BLACKOUT_MS);
    assert!(BADGE_HEARTBEAT_INTERVAL_MS < BADGE_HEARTBEAT_FAILURE_BUDGET_MS);
    // Give up before Temporal does, never after: the server reassigning a
    // question the badge still believes it owns is the one ordering that
    // shows two badges the same live question.
    assert!(BADGE_HEARTBEAT_FAILURE_BUDGET_MS < BADGE_HEARTBEAT_TIMEOUT_MS);
};

/// Whether a badge should give its question up after this long without an
/// acknowledged Activity heartbeat.
///
/// Split out from the RPC call so the decision is testable from a development
/// host; the firmware crate only builds for the badge.
#[must_use]
pub const fn heartbeat_budget_exhausted(since_acknowledged_ms: u64) -> bool {
    since_acknowledged_ms >= BADGE_HEARTBEAT_FAILURE_BUDGET_MS
}
/// Rolling event window carried on every snapshot.
pub const EVENT_WINDOW: usize = 24;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub category: String,
    pub difficulty: String,
    pub prompt: String,
    pub answers: [String; 4],
    #[serde(deserialize_with = "deserialize_answer_index")]
    pub correct_index: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionTask {
    pub game_id: String,
    pub deadline_unix_ms: u64,
    // Zero preserves compatibility with Workflow histories written before the
    // extension field existed. Consumers use
    // `latest_possible_deadline_unix_ms` so an in-flight Activity can survive
    // a possible +30 second Workflow extension.
    #[serde(default)]
    pub max_deadline_unix_ms: u64,
    pub question: Question,
}

impl QuestionTask {
    pub fn latest_possible_deadline_unix_ms(&self) -> u64 {
        self.deadline_unix_ms.max(self.max_deadline_unix_ms)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BadgeAnswer {
    pub badge_id: String,
    pub callsign: String,
    pub question_id: String,
    #[serde(deserialize_with = "deserialize_answer_index")]
    pub selected_index: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BadgeEvent {
    pub badge_id: String,
    pub callsign: String,
    pub question_id: String,
    /// The real Temporal Activity attempt, starting at one.
    #[serde(default = "default_attempt")]
    pub attempt: u32,
}

const fn default_attempt() -> u32 {
    1
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChaosCommand {
    DoublePoints,
    RustOnly,
    SuddenDeath,
    ExtendThirtySeconds,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PowerupNotice {
    pub sequence: u32,
    pub command: ChaosCommand,
    pub issued_unix_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChaosState {
    pub double_points_until_unix_ms: Option<u64>,
    pub rust_only_until_unix_ms: Option<u64>,
    pub sudden_death: bool,
    pub extension_used: bool,
    #[serde(default)]
    pub latest_powerup: Option<PowerupNotice>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reassignment {
    pub question_id: String,
    pub from_callsign: String,
    pub to_callsign: String,
    pub reason: String,
    #[serde(default = "default_attempt")]
    pub attempt: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BadgeFailure {
    pub question_id: String,
    pub callsign: String,
    #[serde(default = "default_attempt")]
    pub attempt: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GameInput {
    pub game_id: String,
    pub questions: Vec<Question>,
    pub duration_seconds: u64,
    pub backlog_override: Option<usize>,
    #[serde(default)]
    pub detected_badge_count: Option<usize>,
    #[serde(default)]
    pub index_search_attributes: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlayerScore {
    pub badge_id: String,
    pub callsign: String,
    pub score: i32,
    pub correct: u32,
    pub wrong: u32,
    pub panics: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GameStatus {
    #[default]
    Waiting,
    Running,
    Finished,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerSpotlight {
    pub question: String,
    pub correct_answer: String,
    pub callsign: String,
    pub was_correct: bool,
    pub score: i32,
    #[serde(default = "default_points")]
    pub points: i32,
}

const fn default_points() -> i32 {
    1
}

/// Why an event happened, so consumers can filter without pattern-matching on
/// English prose. At ten badges routine answers arrive several times a second
/// and would otherwise flush every fault out of any fixed-size feed.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Round lifecycle: started, badge joined, winner, deck exhausted.
    #[default]
    Lifecycle,
    /// Routine gameplay: a badge answered, correctly or not.
    Answer,
    /// A failure: a crash, a heartbeat timeout, or a rejected payload.
    Fault,
    /// Temporal moved unfinished work to a different badge.
    Handoff,
    /// An operator powerup landed as a validated Workflow Update.
    Chaos,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GameEvent {
    pub text: String,
    #[serde(default)]
    pub kind: EventKind,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GameSnapshot {
    pub game_id: Option<String>,
    pub status: GameStatus,
    pub started_unix_ms: Option<u64>,
    pub deadline_unix_ms: Option<u64>,
    pub completed_questions: u32,
    pub scheduled_questions: u32,
    pub players: BTreeMap<String, PlayerScore>,
    #[serde(default)]
    pub detected_badge_count: u32,
    pub latest_answer: Option<AnswerSpotlight>,
    pub events: Vec<GameEvent>,
    pub winners: Vec<String>,
    #[serde(default)]
    pub reassignments: u32,
    #[serde(default)]
    pub heartbeat_timeouts: u32,
    #[serde(default)]
    pub activity_attempts: u32,
    #[serde(default)]
    pub latest_reassignment: Option<Reassignment>,
    #[serde(default)]
    pub latest_failure: Option<BadgeFailure>,
    #[serde(default)]
    pub chaos: ChaosState,
}

impl GameSnapshot {
    /// Records a lifecycle event. Call [`GameSnapshot::push_kind`] when the
    /// event is anything a consumer might want to filter on.
    pub fn push_event(&mut self, text: String) {
        self.push_kind(EventKind::Lifecycle, text);
    }

    pub fn push_kind(&mut self, kind: EventKind, text: String) {
        self.events.push(GameEvent { text, kind });
        while self.events.len() > EVENT_WINDOW {
            // Ten badges produce several answers a second, which would evict
            // every fault, handoff and powerup from the window within a second
            // or two. Routine answers are therefore dropped first, so the
            // durable story survives until the window fills with it.
            let victim = self
                .events
                .iter()
                .position(|event| event.kind == EventKind::Answer)
                .unwrap_or(0);
            self.events.remove(victim);
        }
    }

    /// Closes the round and names every badge on the top score.
    ///
    /// A tie is the normal case, not an edge case: badges answer the same deck
    /// concurrently and land on the same total often. Everyone level at the top
    /// wins, and that holds at zero too -- if the timer beats the first answer,
    /// every badge that joined is tied at nought and every panel reads WINNER.
    /// A scoreless round is still a round that was played to a draw.
    ///
    /// The only round with no winner is one no badge ever joined.
    pub fn finish(&mut self) {
        self.status = GameStatus::Finished;
        let high_score = self.players.values().map(|player| player.score).max();
        self.winners = high_score
            .map(|score| {
                let mut winners = self
                    .players
                    .values()
                    .filter(|player| player.score == score)
                    .map(|player| player.callsign.clone())
                    .collect::<Vec<_>>();
                winners.sort_unstable();
                winners
            })
            .unwrap_or_default();
        if self.winners.is_empty() {
            // Reachable only with an empty roster: any badge that joined has a
            // score, so it ties for the top even if that top is zero.
            self.push_event("Round finished with no badges".to_owned());
        } else {
            self.push_event(format!("Winner: {}", self.winners.join(" + ")));
        }
    }
}

fn deserialize_answer_index<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    let index = u8::deserialize(deserializer)?;
    if index < 4 {
        Ok(index)
    } else {
        Err(de::Error::custom(format_args!(
            "answer index {index} is outside 0..4"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_heartbeat_costs_nothing_until_the_budget_runs_out() {
        // The defect this pins: one failed heartbeat RPC used to end the
        // Activity outright, so a single lost packet took a question away
        // from a player mid-read and showed a fabricated handoff on the TV.
        assert!(!heartbeat_budget_exhausted(0), "the first miss is free");
        assert!(!heartbeat_budget_exhausted(BADGE_HEARTBEAT_INTERVAL_MS));
        assert!(!heartbeat_budget_exhausted(
            BADGE_HEARTBEAT_FAILURE_BUDGET_MS - 1
        ));
        assert!(heartbeat_budget_exhausted(
            BADGE_HEARTBEAT_FAILURE_BUDGET_MS
        ));
        assert!(
            heartbeat_budget_exhausted(BADGE_HEARTBEAT_TIMEOUT_MS),
            "the badge must have given up before Temporal reassigns at {BADGE_HEARTBEAT_TIMEOUT_MS} ms"
        );
    }

    #[test]
    fn rejects_invalid_wire_answer_indexes() {
        let json = r#"{
            "id":"q1","category":"rust","difficulty":"easy","prompt":"?",
            "answers":["a","b","c","d"],"correct_index":4
        }"#;
        assert!(serde_json::from_str::<Question>(json).is_err());
    }

    #[test]
    fn legacy_badge_event_defaults_to_first_activity_attempt() {
        let event: BadgeEvent = serde_json::from_str(
            r#"{"badge_id":"badge-1","callsign":"KEEN-SEAL-70","question_id":"q-1"}"#,
        )
        .expect("legacy BadgeEvent payload");
        assert_eq!(event.attempt, 1);
    }

    fn player(badge_id: &str, score: i32) -> PlayerScore {
        PlayerScore {
            badge_id: badge_id.to_owned(),
            callsign: badge_id.to_uppercase(),
            score,
            ..Default::default()
        }
    }

    #[test]
    fn a_scoreless_round_is_a_tie_between_every_badge() {
        // A scoreless round is a draw, not a void. `badge_started` inserts a
        // player at zero before that badge has answered anything, so if the
        // timer beats the first answer the whole field is level at the top and
        // shares the win.
        let mut state = GameSnapshot::default();
        for badge_id in ["badge-a", "badge-b", "badge-c"] {
            state
                .players
                .insert(badge_id.to_owned(), player(badge_id, 0));
        }
        state.finish();
        assert_eq!(state.winners, ["BADGE-A", "BADGE-B", "BADGE-C"]);
    }

    #[test]
    fn only_a_round_nobody_joined_has_no_winner() {
        let mut state = GameSnapshot::default();
        state.finish();
        assert!(state.winners.is_empty());
        assert_eq!(
            state.events.last().map(|event| event.text.as_str()),
            Some("Round finished with no badges"),
            "an empty roster is the only path that reports no winner"
        );
    }

    #[test]
    fn every_badge_level_at_the_top_shares_the_win() {
        let mut state = GameSnapshot::default();
        state.players.insert("a".to_owned(), player("a", 7));
        state.players.insert("b".to_owned(), player("b", 7));
        state.players.insert("c".to_owned(), player("c", 3));
        state.finish();
        assert_eq!(state.winners, ["A", "B"]);
    }

    #[test]
    fn a_negative_field_still_names_the_least_bad_badge() {
        let mut state = GameSnapshot::default();
        state.players.insert("a".to_owned(), player("a", -1));
        state.players.insert("b".to_owned(), player("b", -6));
        state.finish();
        assert_eq!(state.winners, ["A"]);
    }

    #[test]
    fn the_event_window_drops_answers_before_faults() {
        let mut state = GameSnapshot::default();
        state.push_kind(EventKind::Fault, "first fault".to_owned());
        for index in 0..EVENT_WINDOW * 2 {
            state.push_kind(EventKind::Answer, format!("answer {index}"));
        }
        assert_eq!(state.events.len(), EVENT_WINDOW);
        assert_eq!(
            state.events.first().map(|event| event.text.as_str()),
            Some("first fault"),
            "the fault survived {} routine answers",
            EVENT_WINDOW * 2
        );
    }

    #[test]
    fn the_event_window_evicts_the_oldest_when_nothing_is_an_answer() {
        let mut state = GameSnapshot::default();
        for index in 0..EVENT_WINDOW + 3 {
            state.push_kind(EventKind::Fault, format!("fault {index}"));
        }
        assert_eq!(state.events.len(), EVENT_WINDOW);
        assert_eq!(
            state.events.first().map(|event| event.text.as_str()),
            Some("fault 3"),
            "with no answer to sacrifice it falls back to the oldest event"
        );
    }

    #[test]
    fn an_in_flight_badge_survives_the_thirty_second_extension() {
        let question = Question {
            id: "q-1".to_owned(),
            category: "rust".to_owned(),
            difficulty: "easy".to_owned(),
            prompt: "?".to_owned(),
            answers: [
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "d".to_owned(),
            ],
            correct_index: 0,
        };
        let extended = QuestionTask {
            game_id: "g".to_owned(),
            deadline_unix_ms: 60_000,
            max_deadline_unix_ms: 90_000,
            question: question.clone(),
        };
        assert_eq!(extended.latest_possible_deadline_unix_ms(), 90_000);

        // Histories written before the field existed decode it as zero, which
        // must not pull the ceiling below the real deadline.
        let legacy = QuestionTask {
            max_deadline_unix_ms: 0,
            ..extended
        };
        assert_eq!(legacy.latest_possible_deadline_unix_ms(), 60_000);
    }
}

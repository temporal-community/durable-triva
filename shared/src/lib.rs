use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    fmt,
};

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
/// Rolling event window carried on every snapshot.
pub const EVENT_WINDOW: usize = 24;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for EnvParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}: {}", self.line, self.message)
    }
}

impl Error for EnvParseError {}

pub fn parse_env(content: &str) -> Result<HashMap<String, String>, EnvParseError> {
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            Some((index + 1, line.strip_prefix("export ").unwrap_or(line)))
        })
        .map(|(line_number, line)| {
            let (key, raw_value) = line.split_once('=').ok_or_else(|| EnvParseError {
                line: line_number,
                message: "expected KEY=VALUE".to_owned(),
            })?;
            let key = key.trim();
            if key.is_empty()
                || !key
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
            {
                return Err(EnvParseError {
                    line: line_number,
                    message: format!("invalid environment key {key:?}"),
                });
            }
            Ok((
                key.to_owned(),
                parse_env_value(raw_value.trim(), line_number)?,
            ))
        })
        .collect()
}

fn parse_env_value(value: &str, line: usize) -> Result<String, EnvParseError> {
    let Some(quote) = value
        .chars()
        .next()
        .filter(|character| matches!(character, '\'' | '"'))
    else {
        let comment = value.char_indices().find_map(|(index, character)| {
            (character == '#'
                && (index == 0
                    || value[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace)))
            .then_some(index)
        });
        return Ok(value[..comment.unwrap_or(value.len())].trim().to_owned());
    };

    let mut escaped = false;
    let mut closing_quote = None;
    for (index, character) in value.char_indices().skip(1) {
        if quote == '"' && escaped {
            escaped = false;
            continue;
        }
        if quote == '"' && character == '\\' {
            escaped = true;
        } else if character == quote {
            closing_quote = Some(index);
            break;
        }
    }
    let closing_quote = closing_quote.ok_or_else(|| EnvParseError {
        line,
        message: "unterminated quoted value".to_owned(),
    })?;
    let remainder = value[closing_quote + quote.len_utf8()..].trim();
    if !remainder.is_empty() && !remainder.starts_with('#') {
        return Err(EnvParseError {
            line,
            message: "unexpected text after quoted value".to_owned(),
        });
    }
    let quoted = &value[quote.len_utf8()..closing_quote];
    if quote == '\'' {
        return Ok(quoted.to_owned());
    }
    let mut parsed = String::with_capacity(quoted.len());
    let mut characters = quoted.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            parsed.push(character);
            continue;
        }
        let escaped = characters.next().ok_or_else(|| EnvParseError {
            line,
            message: "trailing escape in quoted value".to_owned(),
        })?;
        parsed.push(match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '\\' => '\\',
            '"' => '"',
            other => other,
        });
    }
    Ok(parsed)
}

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

    pub fn target_backlog(&self, override_value: Option<usize>) -> usize {
        override_value.unwrap_or_else(|| 10.max(self.players.len() * 2))
    }

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
            self.push_event("Round finished with no answers".to_owned());
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
    fn backlog_scales_from_ten() {
        let mut state = GameSnapshot::default();
        assert_eq!(state.target_backlog(None), 10);
        for index in 0..8 {
            state.players.insert(
                index.to_string(),
                PlayerScore {
                    badge_id: index.to_string(),
                    callsign: format!("BADGE-{index}"),
                    ..Default::default()
                },
            );
        }
        assert_eq!(state.target_backlog(None), 16);
        assert_eq!(state.target_backlog(Some(33)), 33);
    }

    #[test]
    fn finish_sorts_tied_winners_by_callsign() {
        let mut state = GameSnapshot::default();
        for (badge_id, callsign) in [("badge-a", "FERRIS-01"), ("badge-z", "CRAB-02")] {
            state.players.insert(
                badge_id.to_owned(),
                PlayerScore {
                    badge_id: badge_id.to_owned(),
                    callsign: callsign.to_owned(),
                    score: 4,
                    ..Default::default()
                },
            );
        }
        state.finish();
        assert_eq!(state.winners, ["CRAB-02", "FERRIS-01"]);
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
    fn parses_common_dotenv_syntax() {
        let values = parse_env(
            "export ONE=plain # comment\nTWO='hash # stays'\nTHREE=\"line\\nvalue\" # comment",
        )
        .unwrap();
        assert_eq!(values["ONE"], "plain");
        assert_eq!(values["TWO"], "hash # stays");
        assert_eq!(values["THREE"], "line\nvalue");
        assert!(parse_env("BROKEN='unterminated").is_err());
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
    fn a_scoreless_round_names_every_badge_that_joined() {
        // Pins current behaviour rather than endorsing it. `badge_started`
        // inserts a player at zero before that badge has answered anything, so
        // a round whose timer beats the first answer leaves the whole field
        // tied at the maximum and every panel reads WINNER.
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
    fn a_round_nobody_joined_has_no_winner() {
        let mut state = GameSnapshot::default();
        state.finish();
        assert!(state.winners.is_empty());
        assert_eq!(
            state.events.last().map(|event| event.text.as_str()),
            Some("Round finished with no answers"),
            "the empty-field message is the only path that reports no winner"
        );
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

    #[test]
    fn an_unquoted_hash_without_leading_space_stays_in_the_value() {
        let values = parse_env("TAG=build#42\nNOTE=value # comment").unwrap();
        assert_eq!(values["TAG"], "build#42");
        assert_eq!(values["NOTE"], "value");
    }
}

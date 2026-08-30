pub use temporal_trivia_shared::{
    AnswerSpotlight, BADGE_TASK_QUEUE, BadgeAnswer, BadgeEvent, BadgeFailure, CHAOS_DURATION_MS,
    ChaosCommand, EventKind, GAME_EXTENSION_MS, GAME_SECONDS, GameInput, GameSnapshot, GameStatus,
    PlayerScore, PowerupNotice, Question, QuestionTask, Reassignment, WEB_TASK_QUEUE,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoundMemo {
    pub game_id: String,
    pub winners: Vec<String>,
    pub badge_count: i64,
    pub correct_answers: i64,
    pub wrong_answers: i64,
    pub crashes: i64,
    pub reassignments: i64,
    #[serde(default)]
    pub heartbeat_timeouts: i64,
    #[serde(default)]
    pub activity_attempts: i64,
}

impl From<&GameSnapshot> for RoundMemo {
    fn from(snapshot: &GameSnapshot) -> Self {
        Self {
            game_id: snapshot.game_id.clone().unwrap_or_default(),
            winners: snapshot.winners.clone(),
            badge_count: snapshot.players.len() as i64,
            correct_answers: snapshot
                .players
                .values()
                .map(|player| i64::from(player.correct))
                .sum(),
            wrong_answers: snapshot
                .players
                .values()
                .map(|player| i64::from(player.wrong))
                .sum(),
            crashes: snapshot
                .players
                .values()
                .map(|player| i64::from(player.panics))
                .sum(),
            reassignments: i64::from(snapshot.reassignments),
            heartbeat_timeouts: i64::from(snapshot.heartbeat_timeouts),
            activity_attempts: i64::from(snapshot.activity_attempts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_round_memos_default_new_temporal_counters() {
        let memo: RoundMemo = serde_json::from_str(
            r#"{"game_id":"g","winners":[],"badge_count":1,"correct_answers":2,"wrong_answers":1,"crashes":0,"reassignments":0}"#,
        )
        .expect("old round memo");
        assert_eq!(memo.heartbeat_timeouts, 0);
        assert_eq!(memo.activity_attempts, 0);
    }

    #[test]
    fn round_memo_totals_every_player() {
        let mut snapshot = GameSnapshot {
            game_id: Some("round-7".to_owned()),
            ..Default::default()
        };
        for (badge_id, correct, wrong, panics) in [("a", 3_u32, 1_u32, 0_u32), ("b", 2, 4, 2)] {
            snapshot.players.insert(
                badge_id.to_owned(),
                PlayerScore {
                    badge_id: badge_id.to_owned(),
                    callsign: badge_id.to_uppercase(),
                    score: i32::try_from(correct).unwrap() - i32::try_from(wrong).unwrap(),
                    correct,
                    wrong,
                    panics,
                },
            );
        }
        snapshot.reassignments = 5;
        snapshot.heartbeat_timeouts = 4;
        snapshot.activity_attempts = 17;
        snapshot.winners = vec!["A".to_owned()];

        let memo = RoundMemo::from(&snapshot);
        assert_eq!(memo.game_id, "round-7");
        assert_eq!(memo.winners, ["A"]);
        assert_eq!(memo.badge_count, 2);
        assert_eq!(memo.correct_answers, 5);
        assert_eq!(memo.wrong_answers, 5);
        assert_eq!(memo.crashes, 2);
        assert_eq!(memo.reassignments, 5);
        assert_eq!(memo.heartbeat_timeouts, 4);
        assert_eq!(memo.activity_attempts, 17);
    }

    #[test]
    fn round_memo_of_an_untouched_snapshot_is_all_zero() {
        let memo = RoundMemo::from(&GameSnapshot::default());
        assert_eq!(memo.game_id, "");
        assert_eq!(memo.badge_count, 0);
        assert_eq!(memo.activity_attempts, 0);
    }
}

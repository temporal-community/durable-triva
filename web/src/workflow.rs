use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use temporalio_common::protos::temporal::api::common::v1::RetryPolicy;
use temporalio_common::search_attributes::SearchAttributeKey;
use temporalio_macros::{activity_definitions, workflow, workflow_methods};
use temporalio_sdk::{
    ActivityOptions, MemoValue, SyncWorkflowContext, WorkflowContext, WorkflowContextView,
    WorkflowResult, activities::ActivityError,
};

use crate::model::{
    AnswerSpotlight, BADGE_TASK_QUEUE, BadgeAnswer, BadgeEvent, BadgeFailure, CHAOS_DURATION_MS,
    ChaosCommand, EventKind, GAME_EXTENSION_MS, GameInput, GameSnapshot, GameStatus, PlayerScore,
    PowerupNotice, Question, QuestionTask, Reassignment, RoundMemo,
};

pub struct BadgeActivities;

#[activity_definitions]
impl BadgeActivities {
    #[activity(name = "trivia.answer_question")]
    fn answer_question(_task: QuestionTask) -> Result<BadgeAnswer, ActivityError> {
        unimplemented!()
    }
}

#[workflow]
#[derive(Default)]
pub struct GameWorkflow {
    snapshot: GameSnapshot,
    assignments: BTreeMap<String, BadgeEvent>,
    retry_reasons: BTreeMap<String, String>,
    attempts_seen: BTreeSet<String>,
    questions: BTreeMap<String, Question>,
    index_search_attributes: bool,
}

pub type GameWorkflowRun = <GameWorkflow as temporalio_common::HasWorkflowDefinition>::Run;

#[workflow_methods]
impl GameWorkflow {
    #[run]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        input: GameInput,
    ) -> WorkflowResult<GameSnapshot> {
        let started_unix_ms = unix_ms(ctx.workflow_time().unwrap_or(UNIX_EPOCH));
        let deadline_unix_ms = started_unix_ms + input.duration_seconds * 1_000;
        ctx.state_mut(|state| {
            state.assignments.clear();
            state.retry_reasons.clear();
            state.attempts_seen.clear();
            state.questions = input
                .questions
                .iter()
                .cloned()
                .map(|question| (question.id.clone(), question))
                .collect();
            state.index_search_attributes = input.index_search_attributes;
            state.snapshot = GameSnapshot {
                game_id: Some(input.game_id.clone()),
                status: GameStatus::Running,
                started_unix_ms: Some(started_unix_ms),
                deadline_unix_ms: Some(deadline_unix_ms),
                ..Default::default()
            };
            state.snapshot.push_event("Round started".to_owned());
            if let Some(badge_count) = input.detected_badge_count {
                let active_slots = input
                    .backlog_override
                    .unwrap_or_else(|| badge_count.saturating_sub(1).max(1));
                if badge_count > 1 {
                    state.snapshot.push_event(format!(
                        "{badge_count} badges ready · {active_slots} playing · 1 recovery reserve"
                    ));
                } else {
                    state
                        .snapshot
                        .push_event("1 badge ready · no recovery reserve".to_owned());
                }
            }
        });
        if input.index_search_attributes {
            upsert_running_search_attributes(ctx);
        }

        type PendingResult = (
            Question,
            Result<BadgeAnswer, temporalio_sdk::ActivityExecutionError>,
        );
        let mut pending: FuturesUnordered<futures::future::LocalBoxFuture<'static, PendingResult>> =
            FuturesUnordered::new();
        let mut available: VecDeque<Question> = input.questions.into();
        let activity_timeout = Duration::from_secs(input.duration_seconds + 35);

        loop {
            let now_unix_ms = workflow_unix_ms(ctx);
            ctx.state_mut(|state| expire_chaos(&mut state.snapshot, now_unix_ms));
            let deadline_unix_ms = ctx
                .state(|state| state.snapshot.deadline_unix_ms)
                .unwrap_or(now_unix_ms);
            if now_unix_ms >= deadline_unix_ms {
                break;
            }
            let rust_only = ctx.state(|state| {
                state
                    .snapshot
                    .chaos
                    .rust_only_until_unix_ms
                    .is_some_and(|until| until > now_unix_ms)
            });
            let target = ctx.state(|state| state.snapshot.target_backlog(input.backlog_override));
            while pending.len() < target {
                let Some(question) = take_next_question(&mut available, rust_only) else {
                    break;
                };
                let task = QuestionTask {
                    game_id: input.game_id.clone(),
                    deadline_unix_ms,
                    // The extension card is single-use, so this upper bound
                    // keeps an in-flight badge alive across a possible +30s.
                    max_deadline_unix_ms: started_unix_ms
                        + input.duration_seconds * 1_000
                        + GAME_EXTENSION_MS,
                    question: question.clone(),
                };
                let activity_ctx = (*ctx).clone();
                pending.push(
                    async move {
                        let result = activity_ctx
                            .execute_activity(
                                BadgeActivities::answer_question,
                                task,
                                ActivityOptions::with_schedule_to_close_timeout(activity_timeout)
                                    .heartbeat_timeout(Duration::from_secs(5))
                                    .retry_policy(RetryPolicy {
                                        initial_interval: Some(prost_wkt_types::Duration {
                                            seconds: 0,
                                            nanos: 250_000_000,
                                        }),
                                        backoff_coefficient: 1.0,
                                        maximum_interval: Some(prost_wkt_types::Duration {
                                            seconds: 1,
                                            nanos: 0,
                                        }),
                                        ..Default::default()
                                    })
                                    .task_queue(BADGE_TASK_QUEUE)
                                    .activity_id(question.id.clone())
                                    .build(),
                            )
                            .await;
                        (question, result)
                    }
                    .boxed_local(),
                );
                ctx.state_mut(|state| state.snapshot.scheduled_questions += 1);
            }

            if pending.is_empty() && available.is_empty() {
                ctx.state_mut(|state| {
                    state
                        .snapshot
                        .push_event("Question deck exhausted".to_owned())
                });
                break;
            }

            let tick_duration = Duration::from_millis((deadline_unix_ms - now_unix_ms).min(1_000));
            let tick_ctx = (*ctx).clone();
            let mut tick = async move { tick_ctx.timer(tick_duration).await }
                .boxed_local()
                .fuse();

            if pending.is_empty() {
                tick.await;
                continue;
            }

            futures::select_biased! {
                _ = tick => continue,
                completed = pending.next().fuse() => {
                    let Some((question, result)) = completed else { break };
                    match result {
                        Ok(answer) => {
                            let now_unix_ms = workflow_unix_ms(ctx);
                            if record_answer(ctx, question, answer, now_unix_ms) {
                                break;
                            }
                        }
                        Err(error) => ctx.state_mut(|state| {
                            state.snapshot.push_kind(
                                EventKind::Fault,
                                format!(
                                    "Question {} closed without an answer: {error}",
                                    question.id
                                ),
                            );
                        }),
                    }
                }
            }
        }

        drop(pending);
        ctx.state_mut(|state| state.snapshot.finish());
        let round_memo = ctx.state(|state| RoundMemo::from(&state.snapshot));
        ctx.upsert_memo([(
            "TriviaRoundSummary".to_owned(),
            Some(MemoValue::new(round_memo)),
        )])
        .expect("round summary memo update");
        if input.index_search_attributes {
            upsert_finished_search_attributes(ctx);
        }
        Ok(ctx.state(|state| state.snapshot.clone()))
    }

    #[signal]
    pub fn badge_started(&mut self, ctx: &mut SyncWorkflowContext<Self>, event: BadgeEvent) {
        if let Some(previous) = self.assignments.get(&event.question_id)
            && is_reassignment(previous, &event)
        {
            // Attempt is authoritative Temporal data. A failed Worker may be
            // unable to send the best-effort panic Signal, so never require
            // that Signal to recognize the retry.
            let reason = self
                .retry_reasons
                .remove(&event.question_id)
                .unwrap_or_else(|| "heartbeat timeout".to_owned());
            let reassignment = Reassignment {
                question_id: event.question_id.clone(),
                from_callsign: previous.callsign.clone(),
                to_callsign: event.callsign.clone(),
                reason,
                attempt: event.attempt,
            };
            self.snapshot.reassignments += 1;
            self.snapshot.heartbeat_timeouts += 1;
            self.snapshot.latest_reassignment = Some(reassignment.clone());
            self.snapshot.push_kind(
                EventKind::Handoff,
                // The question id distinguishes two handoffs that otherwise
                // read identically in the feed.
                format!(
                    "WORK REASSIGNED · {} · {} -> {} · ATTEMPT {} · {}",
                    reassignment.question_id,
                    reassignment.from_callsign,
                    reassignment.to_callsign,
                    reassignment.attempt,
                    reassignment.reason
                ),
            );
        }
        self.assignments
            .insert(event.question_id.clone(), event.clone());
        if self
            .attempts_seen
            .insert(format!("{}:{}", event.question_id, event.attempt))
        {
            self.snapshot.activity_attempts += 1;
        }
        if !self.snapshot.players.contains_key(&event.badge_id) {
            self.snapshot.players.insert(
                event.badge_id.clone(),
                PlayerScore {
                    badge_id: event.badge_id,
                    callsign: event.callsign.clone(),
                    ..Default::default()
                },
            );
            self.snapshot
                .push_event(format!("{} joined", event.callsign));
        }
        if self.index_search_attributes {
            ctx.upsert_search_attributes([
                SearchAttributeKey::int("TriviaBadgeCount")
                    .value_set(self.snapshot.players.len() as i64),
                SearchAttributeKey::int("TriviaReassignments")
                    .value_set(self.snapshot.reassignments as i64),
            ]);
        }
    }

    #[signal]
    pub fn panic_event(&mut self, _ctx: &mut SyncWorkflowContext<Self>, event: BadgeEvent) {
        self.retry_reasons
            .insert(event.question_id.clone(), "heartbeat timeout".to_owned());
        self.snapshot.latest_failure = Some(BadgeFailure {
            question_id: event.question_id.clone(),
            callsign: event.callsign.clone(),
            attempt: event.attempt,
        });
        let player = self
            .snapshot
            .players
            .entry(event.badge_id.clone())
            .or_insert_with(|| PlayerScore {
                badge_id: event.badge_id,
                callsign: event.callsign.clone(),
                ..Default::default()
            });
        player.panics += 1;
        self.snapshot.push_kind(
            EventKind::Fault,
            format!("{} crashed on {}", event.callsign, event.question_id),
        );
    }

    #[signal]
    pub fn recovered(&mut self, _ctx: &mut SyncWorkflowContext<Self>, event: BadgeEvent) {
        self.snapshot.push_kind(
            EventKind::Handoff,
            format!("{} recovered; question returned", event.callsign),
        );
    }

    #[update_validator(apply_chaos)]
    fn validate_apply_chaos(
        &self,
        _ctx: &WorkflowContextView,
        command: &ChaosCommand,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.snapshot.status != GameStatus::Running {
            return Err("no game is running".into());
        }
        if *command == ChaosCommand::ExtendThirtySeconds {
            return if self.snapshot.chaos.extension_used {
                Err("the +30 second extension was already used".into())
            } else {
                Ok(())
            };
        }
        if let Some(active) = active_modifier(&self.snapshot) {
            return Err(
                format!("{active} is already active; gameplay modifiers cannot overlap").into(),
            );
        }
        Ok(())
    }

    #[update]
    pub fn apply_chaos(
        &mut self,
        ctx: &mut SyncWorkflowContext<Self>,
        command: ChaosCommand,
    ) -> GameSnapshot {
        let now_unix_ms = unix_ms(ctx.workflow_time().unwrap_or(UNIX_EPOCH));
        match command {
            ChaosCommand::DoublePoints => {
                self.snapshot.chaos.double_points_until_unix_ms =
                    Some(now_unix_ms + CHAOS_DURATION_MS);
                self.snapshot.push_kind(
                    EventKind::Chaos,
                    "CHAOS: double points for 10 seconds".to_owned(),
                );
            }
            ChaosCommand::RustOnly => {
                self.snapshot.chaos.rust_only_until_unix_ms = Some(now_unix_ms + CHAOS_DURATION_MS);
                self.snapshot.push_kind(
                    EventKind::Chaos,
                    "CHAOS: Rust questions only for 10 seconds".to_owned(),
                );
            }
            ChaosCommand::SuddenDeath => {
                self.snapshot.chaos.sudden_death = true;
                self.snapshot.push_kind(
                    EventKind::Chaos,
                    "CHAOS: next correct answer ends the round".to_owned(),
                );
            }
            ChaosCommand::ExtendThirtySeconds => {
                self.snapshot.chaos.extension_used = true;
                self.snapshot.deadline_unix_ms = self
                    .snapshot
                    .deadline_unix_ms
                    .map(|deadline| deadline + GAME_EXTENSION_MS);
                self.snapshot.push_kind(
                    EventKind::Chaos,
                    "CHAOS: Temporal timer extended by 30 seconds".to_owned(),
                );
            }
        }
        let sequence = self
            .snapshot
            .chaos
            .latest_powerup
            .as_ref()
            .map_or(1, |notice| notice.sequence.saturating_add(1));
        self.snapshot.chaos.latest_powerup = Some(PowerupNotice {
            sequence,
            command,
            issued_unix_ms: now_unix_ms,
        });
        self.snapshot.clone()
    }

    #[update_validator(end_round)]
    fn validate_end_round(
        &self,
        _ctx: &WorkflowContextView,
        _input: &(),
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.snapshot.status != GameStatus::Running {
            return Err("no game is running".into());
        }
        Ok(())
    }

    #[update]
    pub fn end_round(&mut self, ctx: &mut SyncWorkflowContext<Self>, _input: ()) -> GameSnapshot {
        let now_unix_ms = unix_ms(ctx.workflow_time().unwrap_or(UNIX_EPOCH));
        // The run loop re-reads the deadline on every tick, so bringing it
        // forward closes the round within one tick. No second control path, and
        // the round still completes through the normal finish path.
        self.snapshot.deadline_unix_ms = Some(now_unix_ms);
        self.snapshot
            .push_event("Operator ended the round early".to_owned());
        self.snapshot.clone()
    }

    #[query]
    pub fn snapshot(&self, _ctx: &WorkflowContextView) -> GameSnapshot {
        self.snapshot.clone()
    }
}

fn record_answer(
    ctx: &mut WorkflowContext<GameWorkflow>,
    question: Question,
    answer: BadgeAnswer,
    now_unix_ms: u64,
) -> bool {
    ctx.state_mut(|state| {
        if answer.question_id != question.id || answer.selected_index > 3 {
            state.snapshot.push_kind(
                EventKind::Fault,
                format!("Rejected malformed answer from {}", answer.callsign),
            );
            return false;
        }
        let was_correct = answer.selected_index == question.correct_index;
        let points = active_points(&state.snapshot, now_unix_ms);
        let score = {
            let player = state
                .snapshot
                .players
                .entry(answer.badge_id.clone())
                .or_insert_with(|| PlayerScore {
                    badge_id: answer.badge_id,
                    callsign: answer.callsign.clone(),
                    ..Default::default()
                });
            if was_correct {
                player.score += points;
                player.correct += 1;
            } else {
                player.score -= points;
                player.wrong += 1;
            }
            player.score
        };
        state.snapshot.completed_questions += 1;
        state.snapshot.latest_answer = Some(AnswerSpotlight {
            question: question.prompt,
            correct_answer: question.answers[question.correct_index as usize].clone(),
            callsign: answer.callsign.clone(),
            was_correct,
            score,
            points,
        });
        state.snapshot.push_kind(
            EventKind::Answer,
            format!(
                "{} answered {} ({:+})",
                answer.callsign,
                if was_correct { "correctly" } else { "wrong" },
                if was_correct { points } else { -points }
            ),
        );
        state.snapshot.chaos.sudden_death && was_correct
    })
}

fn active_points(snapshot: &GameSnapshot, now_unix_ms: u64) -> i32 {
    if snapshot
        .chaos
        .double_points_until_unix_ms
        .is_some_and(|until| until > now_unix_ms)
    {
        2
    } else {
        1
    }
}

fn is_reassignment(previous: &BadgeEvent, current: &BadgeEvent) -> bool {
    previous.badge_id != current.badge_id && current.attempt > previous.attempt
}

fn active_modifier(snapshot: &GameSnapshot) -> Option<&'static str> {
    if snapshot.chaos.double_points_until_unix_ms.is_some() {
        Some("double points")
    } else if snapshot.chaos.rust_only_until_unix_ms.is_some() {
        Some("Rust only")
    } else if snapshot.chaos.sudden_death {
        Some("sudden death")
    } else {
        None
    }
}

fn expire_chaos(snapshot: &mut GameSnapshot, now_unix_ms: u64) {
    if snapshot
        .chaos
        .double_points_until_unix_ms
        .is_some_and(|until| until <= now_unix_ms)
    {
        snapshot.chaos.double_points_until_unix_ms = None;
    }
    if snapshot
        .chaos
        .rust_only_until_unix_ms
        .is_some_and(|until| until <= now_unix_ms)
    {
        snapshot.chaos.rust_only_until_unix_ms = None;
    }
}

fn take_next_question(available: &mut VecDeque<Question>, rust_only: bool) -> Option<Question> {
    if rust_only {
        let index = available
            .iter()
            .position(|question| question.category == "rust")?;
        available.remove(index)
    } else {
        available.pop_front()
    }
}

fn workflow_unix_ms(ctx: &WorkflowContext<GameWorkflow>) -> u64 {
    unix_ms(ctx.workflow_time().unwrap_or(UNIX_EPOCH))
}

fn upsert_running_search_attributes(ctx: &WorkflowContext<GameWorkflow>) {
    ctx.upsert_search_attributes([
        SearchAttributeKey::keyword("TriviaGameStatus").value_set("Running".to_owned()),
        SearchAttributeKey::int("TriviaBadgeCount").value_set(0),
        SearchAttributeKey::int("TriviaReassignments").value_set(0),
        SearchAttributeKey::keyword("TriviaWinner").value_set(String::new()),
        SearchAttributeKey::bool("TriviaRustSdk").value_set(true),
    ]);
}

fn upsert_finished_search_attributes(ctx: &WorkflowContext<GameWorkflow>) {
    let snapshot = ctx.state(|state| state.snapshot.clone());
    ctx.upsert_search_attributes([
        SearchAttributeKey::keyword("TriviaGameStatus").value_set("Finished".to_owned()),
        SearchAttributeKey::keyword("TriviaWinner").value_set(snapshot.winners.join(" + ")),
        SearchAttributeKey::int("TriviaBadgeCount").value_set(snapshot.players.len() as i64),
        SearchAttributeKey::int("TriviaReassignments").value_set(snapshot.reassignments as i64),
    ]);
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(id: &str, category: &str) -> Question {
        Question {
            id: id.to_owned(),
            category: category.to_owned(),
            difficulty: "easy".to_owned(),
            prompt: format!("Question {id}"),
            answers: [
                "A".to_owned(),
                "B".to_owned(),
                "C".to_owned(),
                "D".to_owned(),
            ],
            correct_index: 0,
        }
    }

    #[test]
    fn unix_time_conversion_is_milliseconds() {
        assert_eq!(unix_ms(UNIX_EPOCH + Duration::from_secs(3)), 3_000);
    }

    #[test]
    fn rust_only_scheduling_preserves_other_questions_for_later() {
        let mut available = VecDeque::from([
            question("general-1", "general"),
            question("rust-1", "rust"),
            question("math-1", "math"),
        ]);
        assert_eq!(
            take_next_question(&mut available, true).map(|question| question.id),
            Some("rust-1".to_owned())
        );
        assert_eq!(
            take_next_question(&mut available, false).map(|question| question.id),
            Some("general-1".to_owned())
        );
        assert_eq!(
            available.front().map(|question| question.id.as_str()),
            Some("math-1")
        );
    }

    #[test]
    fn double_points_expires_on_workflow_time() {
        let mut snapshot = GameSnapshot::default();
        snapshot.chaos.double_points_until_unix_ms = Some(20_000);
        assert_eq!(active_points(&snapshot, 19_999), 2);
        assert_eq!(active_points(&snapshot, 20_000), 1);
    }

    #[test]
    fn gameplay_modifiers_are_mutually_exclusive_but_extension_is_independent() {
        let mut snapshot = GameSnapshot::default();
        snapshot.chaos.rust_only_until_unix_ms = Some(20_000);
        assert_eq!(active_modifier(&snapshot), Some("Rust only"));
        expire_chaos(&mut snapshot, 20_000);
        assert_eq!(active_modifier(&snapshot), None);
        snapshot.chaos.extension_used = true;
        assert_eq!(active_modifier(&snapshot), None);
    }

    #[test]
    fn temporal_attempt_number_proves_reassignment_without_panic_signal() {
        let previous = BadgeEvent {
            badge_id: "badge-a".to_owned(),
            callsign: "A".to_owned(),
            question_id: "q".to_owned(),
            attempt: 1,
        };
        let retry = BadgeEvent {
            badge_id: "badge-b".to_owned(),
            callsign: "B".to_owned(),
            question_id: "q".to_owned(),
            attempt: 2,
        };
        assert!(is_reassignment(&previous, &retry));
        assert!(!is_reassignment(
            &previous,
            &BadgeEvent {
                attempt: 1,
                ..retry
            }
        ));
    }

    #[test]
    fn an_expired_modifier_is_not_reported_as_active() {
        // Review finding L2. `validate_apply_chaos` rejects a new powerup with
        // "<X> is already active" whenever `active_modifier` returns Some, but
        // `active_modifier` only tests `.is_some()` and never compares against
        // the clock. `expire_chaos` clears the field once per loop tick, so for
        // up to a second after a modifier expires the operator's next powerup
        // is rejected by an Update validator citing a modifier that has ended.
        let mut snapshot = GameSnapshot::default();
        snapshot.chaos.double_points_until_unix_ms = Some(1_000);
        let now_unix_ms = 5_000;

        // The time-aware half of the pair has already stopped doubling.
        assert_eq!(active_points(&snapshot, now_unix_ms), 1);

        // So the validator must not still call the round modified.
        assert_eq!(
            active_modifier(&snapshot),
            None,
            "double points expired at 1000, it is now {now_unix_ms}"
        );
    }

    #[test]
    fn rust_only_scheduling_falls_back_when_the_rust_pool_is_empty() {
        let mut available: VecDeque<Question> = [
            question("general-1", "general"),
            question("general-2", "general"),
        ]
        .into_iter()
        .collect();
        assert!(
            take_next_question(&mut available, true).is_none(),
            "rust-only must not silently deal a non-rust question"
        );
        assert_eq!(available.len(), 2, "a refused deal leaves the deck intact");
        assert_eq!(
            take_next_question(&mut available, false).map(|q| q.id),
            Some("general-1".to_owned())
        );
    }

    #[test]
    fn expire_chaos_leaves_a_live_modifier_alone() {
        let mut snapshot = GameSnapshot::default();
        snapshot.chaos.double_points_until_unix_ms = Some(10_000);
        snapshot.chaos.rust_only_until_unix_ms = Some(4_000);
        expire_chaos(&mut snapshot, 4_000);
        assert_eq!(
            snapshot.chaos.double_points_until_unix_ms,
            Some(10_000),
            "still in the future"
        );
        assert_eq!(
            snapshot.chaos.rust_only_until_unix_ms, None,
            "expiry is inclusive of the boundary"
        );
    }

    #[test]
    fn a_retry_on_the_same_badge_is_not_a_reassignment() {
        let previous = BadgeEvent {
            badge_id: "badge-a".to_owned(),
            callsign: "A".to_owned(),
            question_id: "q".to_owned(),
            attempt: 1,
        };
        assert!(
            !is_reassignment(
                &previous,
                &BadgeEvent {
                    attempt: 2,
                    ..previous.clone()
                }
            ),
            "same badge picking its own question back up is a retry, not a handoff"
        );
    }
}

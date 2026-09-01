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
    AnswerSpotlight, BADGE_HEARTBEAT_TIMEOUT_MS, BADGE_TASK_QUEUE, BadgeAnswer, BadgeEvent,
    BadgeFailure, CHAOS_DURATION_MS, ChaosCommand, EventKind, GAME_EXTENSION_MS, GameInput,
    GameSnapshot, GameStatus, PlayerScore, PowerupNotice, Question, QuestionTask, Reassignment,
    RoundMemo,
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
    index_search_attributes: bool,
    /// Workflow time as of the last `expire_chaos` sweep.
    ///
    /// `WorkflowContextView` deliberately exposes no clock — an Update
    /// validator has to be a pure function of state and input — so this is
    /// the freshest time `validate_apply_chaos` can reason about. Derived
    /// from the run loop's deterministic `workflow_time`, so it replays.
    last_chaos_sweep_unix_ms: u64,
}

pub type GameWorkflowRun = <GameWorkflow as temporalio_common::HasWorkflowDefinition>::Run;
const FULL_PARTICIPANT_BACKLOG_PATCH: &str = "full-participant-backlog-v1";
/// Recycle the deck when nothing left in it is eligible, not only when it is
/// empty. Rust-only scheduling can leave a full deck with no Rust card in it,
/// and only dealing can empty a deck -- so the pre-patch rule starved every
/// badge for the modifier's whole ten seconds.
const ELIGIBLE_DECK_REFILL_PATCH: &str = "eligible-deck-refill-v1";

#[workflow_methods]
impl GameWorkflow {
    #[run]
    pub async fn run(
        ctx: &mut WorkflowContext<Self>,
        input: GameInput,
    ) -> WorkflowResult<GameSnapshot> {
        let full_participant_backlog = ctx.patched(FULL_PARTICIPANT_BACKLOG_PATCH);
        let eligible_deck_refill = ctx.patched(ELIGIBLE_DECK_REFILL_PATCH);
        let started_unix_ms = unix_ms(ctx.workflow_time().unwrap_or(UNIX_EPOCH));
        let deadline_unix_ms = started_unix_ms + input.duration_seconds * 1_000;
        ctx.state_mut(|state| {
            state.assignments.clear();
            state.retry_reasons.clear();
            state.attempts_seen.clear();
            state.index_search_attributes = input.index_search_attributes;
            state.snapshot = GameSnapshot {
                game_id: Some(input.game_id.clone()),
                status: GameStatus::Running,
                started_unix_ms: Some(started_unix_ms),
                deadline_unix_ms: Some(deadline_unix_ms),
                detected_badge_count: input.detected_badge_count.unwrap_or_default() as u32,
                ..Default::default()
            };
            state.snapshot.push_event("Round started".to_owned());
            if let Some(badge_count) = input.detected_badge_count {
                let active_slots = input.backlog_override.unwrap_or_else(|| {
                    if full_participant_backlog {
                        badge_count
                    } else {
                        badge_count.saturating_sub(1).max(1)
                    }
                });
                if badge_count == 0 {
                    state
                        .snapshot
                        .push_event("No badges are polling · nothing to schedule".to_owned());
                } else {
                    state.snapshot.push_event(format!(
                        "{badge_count} badges ready · {active_slots} question slots"
                    ));
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
        let question_template = input.questions.clone();
        let mut available: VecDeque<Question> = input.questions.into();
        let mut deck_cycle = 1_u32;
        let activity_timeout = Duration::from_secs(input.duration_seconds + 35);
        let mut pending_activities = 0_usize;

        loop {
            let now_unix_ms = workflow_unix_ms(ctx);
            ctx.state_mut(|state| {
                expire_chaos(&mut state.snapshot, now_unix_ms);
                state.last_chaos_sweep_unix_ms = now_unix_ms;
            });
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
            let activity_target = ctx.state(|state| {
                activity_target(
                    &state.snapshot,
                    input.backlog_override,
                    full_participant_backlog,
                )
            });
            while pending_activities < activity_target {
                let previous_cycle = deck_cycle;
                let Some(question) = deal_question(
                    &mut available,
                    &question_template,
                    &mut deck_cycle,
                    rust_only,
                    eligible_deck_refill,
                ) else {
                    break;
                };
                if deck_cycle != previous_cycle {
                    ctx.state_mut(|state| {
                        state.snapshot.push_event(format!(
                            "Question deck recycled; starting cycle {deck_cycle}"
                        ))
                    });
                }
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
                                    .heartbeat_timeout(Duration::from_millis(
                                        BADGE_HEARTBEAT_TIMEOUT_MS,
                                    ))
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
                pending_activities += 1;
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
                    pending_activities = pending_activities.saturating_sub(1);
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
        // A memo is a reporting convenience. Panicking here fails the Workflow
        // Task, which Temporal then retries forever on a round that has already
        // played out -- losing the result to protect a summary field.
        if let Err(error) = ctx.upsert_memo([(
            "TriviaRoundSummary".to_owned(),
            Some(MemoValue::new(round_memo)),
        )]) {
            ctx.state_mut(|state| {
                state.snapshot.push_kind(
                    EventKind::Fault,
                    format!("Round summary memo was not recorded: {error}"),
                );
            });
        }
        if input.index_search_attributes {
            upsert_finished_search_attributes(ctx);
        }
        Ok(ctx.state(|state| state.snapshot.clone()))
    }

    #[signal]
    pub fn badge_started(&mut self, ctx: &mut SyncWorkflowContext<Self>, event: BadgeEvent) {
        let reassigned = self.record_activity_started(event.clone());
        let joined = !self.snapshot.players.contains_key(&event.badge_id);
        if joined {
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
        if should_upsert_running_attributes(self.index_search_attributes, joined, reassigned) {
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
        self.record_panic(event);
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
        if let Some(active) = active_modifier(&self.snapshot, self.last_chaos_sweep_unix_ms) {
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

impl GameWorkflow {
    fn record_panic(&mut self, event: BadgeEvent) {
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

    fn record_activity_started(&mut self, event: BadgeEvent) -> bool {
        let mut reassigned = false;
        if let Some(previous) = self.assignments.get(&event.question_id)
            && is_reassignment(previous, &event)
        {
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
            reassigned = true;
            self.snapshot.reassignments += 1;
            self.snapshot.heartbeat_timeouts += 1;
            self.snapshot.latest_reassignment = Some(reassignment.clone());
            self.snapshot.push_kind(
                EventKind::Handoff,
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
        reassigned
    }
}

fn record_answer(
    ctx: &mut WorkflowContext<GameWorkflow>,
    question: Question,
    answer: BadgeAnswer,
    now_unix_ms: u64,
) -> bool {
    ctx.state_mut(|state| {
        if answer.question_id != question.id
            || usize::from(answer.selected_index) >= question.answers.len()
        {
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
            // correct_index is validated at the serde boundary, but this is
            // the only place it indexes, and a panic here would fail the
            // Workflow Task on a round that has already been answered.
            correct_answer: question
                .answers
                .get(usize::from(question.correct_index))
                .cloned()
                .unwrap_or_default(),
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

/// How many question Activities the Workflow keeps outstanding.
///
/// One per badge detected at round start, so every connected badge keeps
/// playing. Pre-patch histories held one slot back as a recovery reserve and
/// must keep replaying that.
fn activity_target(
    snapshot: &GameSnapshot,
    override_value: Option<usize>,
    full_participant_backlog: bool,
) -> usize {
    let badges = snapshot.detected_badge_count as usize;
    override_value.unwrap_or_else(|| {
        if full_participant_backlog {
            badges
        } else if badges == 0 {
            0
        } else {
            badges.saturating_sub(1).max(1)
        }
    })
}

/// Whether a `badge_started` Signal actually changed either indexed value.
///
/// `TriviaBadgeCount` moves only when a badge joins and `TriviaReassignments`
/// only on a handoff, but the Signal fires once per Activity attempt --
/// roughly 245 times in a full round against about ten joins. Upserting
/// regardless wrote the same numbers back ~225 times per round, each costing
/// a History event and a visibility-store write.
fn should_upsert_running_attributes(indexing: bool, joined: bool, reassigned: bool) -> bool {
    indexing && (joined || reassigned)
}

fn is_reassignment(previous: &BadgeEvent, current: &BadgeEvent) -> bool {
    previous.badge_id != current.badge_id && current.attempt > previous.attempt
}

/// The gameplay modifier in force at `now_unix_ms`, if any.
///
/// Takes the clock rather than trusting `expire_chaos` to have run. That
/// sweep happens once per loop tick, so between a modifier expiring and the
/// next tick the fields are still populated — and this is what
/// `validate_apply_chaos` rejects an operator's powerup on.
fn active_modifier(snapshot: &GameSnapshot, now_unix_ms: u64) -> Option<&'static str> {
    if snapshot
        .chaos
        .double_points_until_unix_ms
        .is_some_and(|until| until > now_unix_ms)
    {
        Some("double points")
    } else if snapshot
        .chaos
        .rust_only_until_unix_ms
        .is_some_and(|until| until > now_unix_ms)
    {
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

/// Deals one question, recycling the deck when it cannot serve the request.
///
/// Pre-patch this recycled only once the deck was completely empty. Rust-only
/// scheduling breaks that rule: it deals nothing but Rust cards, so the deck
/// fills up with everything else and can never empty -- and the field starves
/// for the modifier's whole ten seconds. Post-patch, a deck that has nothing
/// *eligible* is recycled too, and the undealt remainder is kept rather than
/// discarded.
fn deal_question(
    available: &mut VecDeque<Question>,
    template: &[Question],
    deck_cycle: &mut u32,
    rust_only: bool,
    eligible_deck_refill: bool,
) -> Option<Question> {
    if let Some(question) = take_next_question(available, rust_only) {
        return Some(question);
    }
    // Recycling needs a deck to copy from. Nothing else can help.
    if template.is_empty() {
        return None;
    }
    // Replaying a pre-patch history must not deal from a cycle it never
    // recorded, so there the empty-deck rule still governs.
    if !eligible_deck_refill && !available.is_empty() {
        return None;
    }
    *deck_cycle = deck_cycle.saturating_add(1);
    available.extend(refill_question_deck(template, *deck_cycle));
    take_next_question(available, rust_only)
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

fn refill_question_deck(template: &[Question], cycle: u32) -> VecDeque<Question> {
    let mut questions = template.to_vec();
    if !questions.is_empty() {
        let offset = cycle as usize * 97 % questions.len();
        questions.rotate_left(offset);
        if cycle.is_multiple_of(2) {
            questions.reverse();
        }
    }
    questions
        .into_iter()
        .map(|mut question| {
            question.id = format!("{}-cycle-{cycle}", question.id);
            question
        })
        .collect()
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

pub(crate) fn unix_ms(time: SystemTime) -> u64 {
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
    fn every_connected_badge_gets_a_question_slot() {
        let snapshot = GameSnapshot {
            detected_badge_count: 2,
            ..Default::default()
        };
        assert_eq!(activity_target(&snapshot, None, true), 2);
        assert_eq!(
            activity_target(&snapshot, None, false),
            1,
            "pre-patch histories retain their recorded command sequence"
        );
        assert_eq!(
            activity_target(&snapshot, Some(1), true),
            1,
            "an explicit override still wins"
        );
        assert_eq!(
            activity_target(&GameSnapshot::default(), None, true),
            0,
            "no badges detected schedules nothing"
        );
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
    fn every_deck_cycle_produces_unique_activity_ids() {
        // Activity IDs are question IDs, so a collision across cycles would
        // point two Activities at one ID. Cycle one deals the template's own
        // IDs, so the suffix has to keep every later cycle clear of them.
        let template = [question("q-1", "rust"), question("q-2", "general")];
        let mut seen = template
            .iter()
            .map(|question| question.id.clone())
            .collect::<BTreeSet<_>>();
        for cycle in 2..=6 {
            let recycled = refill_question_deck(&template, cycle);
            assert_eq!(recycled.len(), template.len());
            for question in &recycled {
                assert!(
                    seen.insert(question.id.clone()),
                    "cycle {cycle} reused Activity ID {}",
                    question.id
                );
            }
            assert_eq!(
                recycled
                    .iter()
                    .map(|question| question.prompt.as_str())
                    .collect::<BTreeSet<_>>(),
                template
                    .iter()
                    .map(|question| question.prompt.as_str())
                    .collect::<BTreeSet<_>>(),
                "a cycle deals the same questions, only under fresh IDs"
            );
        }
    }

    #[test]
    fn the_deck_is_recycled_only_once_it_is_empty() {
        // The ordinary rule, unchanged by the patch: a deck with cards left
        // deals from those cards and stays on its cycle.
        let template = [question("q-1", "rust"), question("q-2", "general")];
        let mut available: VecDeque<Question> = template.to_vec().into();
        let mut cycle = 1;
        for _ in 0..template.len() {
            assert!(deal_question(&mut available, &template, &mut cycle, false, true).is_some());
        }
        assert_eq!(cycle, 1, "the deck served both deals on its own");
        assert!(deal_question(&mut available, &template, &mut cycle, false, true).is_some());
        assert_eq!(cycle, 2, "and recycles only once it is empty");
    }

    #[test]
    fn rust_only_recycles_the_deck_instead_of_starving_the_field() {
        // The defect: `rust_only` deals nothing but Rust cards, so a deck that
        // has run out of them fills with everything else and can never empty.
        // Waiting for empty left every badge with no question at all for
        // the modifier's full ten seconds.
        let template = [question("rust-1", "rust"), question("general-1", "general")];
        let mut available: VecDeque<Question> = VecDeque::from([question("general-2", "general")]);
        let mut cycle = 1;

        let dealt = deal_question(&mut available, &template, &mut cycle, true, true)
            .expect("a recycled deck can serve a Rust-only deal");
        assert_eq!(dealt.category, "rust");
        assert_eq!(cycle, 2);
        assert!(
            available.iter().any(|question| question.id == "general-2"),
            "the undealt remainder survives the recycle instead of being thrown away"
        );
    }

    #[test]
    fn a_pre_patch_history_still_waits_for_an_empty_deck() {
        // Replay must reproduce the recorded command sequence, so the old
        // starving behaviour has to survive for histories written before the
        // patch marker.
        let template = [question("rust-1", "rust")];
        let mut available: VecDeque<Question> = VecDeque::from([question("general-2", "general")]);
        let mut cycle = 1;
        assert!(
            deal_question(&mut available, &template, &mut cycle, true, false).is_none(),
            "pre-patch, a non-empty deck with nothing eligible deals nothing"
        );
        assert_eq!(cycle, 1, "and records no new cycle");
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
        assert_eq!(active_modifier(&snapshot, 19_999), Some("Rust only"));
        expire_chaos(&mut snapshot, 20_000);
        assert_eq!(active_modifier(&snapshot, 20_000), None);
        snapshot.chaos.extension_used = true;
        assert_eq!(active_modifier(&snapshot, 20_000), None);
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
            active_modifier(&snapshot, now_unix_ms),
            None,
            "double points expired at 1000, it is now {now_unix_ms}"
        );
    }

    #[test]
    fn rust_only_refuses_to_deal_a_non_rust_question() {
        // This is the raw deal, with no deck behind it. `deal_question` is
        // what turns this refusal into a recycle; on its own it must never
        // quietly hand out a question the modifier excludes.
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
    fn an_out_of_range_answer_index_is_rejected_not_indexed() {
        let question = question("q-1", "rust");
        // The wire type validates this at the serde boundary, so an
        // out-of-range index can only arrive from a replayed History or a
        // hand-crafted payload -- exactly when a panic would be worst.
        for index in [4_u8, 200, u8::MAX] {
            assert!(
                usize::from(index) >= question.answers.len(),
                "index {index} must be treated as malformed"
            );
        }
        assert!(usize::from(3_u8) < question.answers.len());
    }

    #[test]
    fn running_attributes_are_upserted_only_when_a_value_changed() {
        // The Signal fires once per Activity attempt; the indexed values move
        // on a join or a handoff and nothing else.
        assert!(should_upsert_running_attributes(true, true, false), "join");
        assert!(
            should_upsert_running_attributes(true, false, true),
            "handoff"
        );
        assert!(
            should_upsert_running_attributes(true, true, true),
            "a joining badge picking up reassigned work"
        );
        assert!(
            !should_upsert_running_attributes(true, false, false),
            "a routine attempt by a known badge changes neither value"
        );
        assert!(
            !should_upsert_running_attributes(false, true, true),
            "indexing off suppresses the upsert regardless"
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

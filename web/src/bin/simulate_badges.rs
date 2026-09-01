use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use temporal_trivia_shared::{
    BADGE_TASK_QUEUE, BadgeAnswer, BadgeEvent, GameInput, GameSnapshot, QuestionTask,
};
use temporal_trivia_web::cloud;
use temporalio_client::{Client, WorkflowSignalOptions};
use temporalio_common::worker::WorkerDeploymentOptions;
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    Runtime, SyncWorkflowContext, Worker, WorkerOptions, WorkflowContext, WorkflowContextView,
    WorkflowResult,
    activities::{ActivityContext, ActivityError},
    runtime::RuntimeOptions,
};
use temporalio_sdk_core::{ActivitySlotKind, FixedSizeSlotSupplier, TunerBuilder};

const MAX_BADGE_INDEX: usize = 100;

#[workflow]
#[derive(Default)]
struct GameWorkflow;

#[workflow_methods]
impl GameWorkflow {
    #[run]
    async fn run(
        _ctx: &mut WorkflowContext<Self>,
        _input: GameInput,
    ) -> WorkflowResult<GameSnapshot> {
        unreachable!("the simulator only registers Activity Workers")
    }

    #[signal]
    fn badge_started(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[signal]
    fn panic_event(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[signal]
    fn recovered(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}

    #[query]
    fn snapshot(&self, _ctx: &WorkflowContextView) -> GameSnapshot {
        GameSnapshot::default()
    }
}

struct SimulatedBadge {
    badge_id: String,
    callsign: String,
    answer_delay: Duration,
    answer_offset: u64,
    answers_seen: AtomicU64,
}

#[activities]
impl SimulatedBadge {
    #[activity(name = "trivia.answer_question")]
    #[allow(dead_code)]
    async fn answer_question(
        self: Arc<Self>,
        ctx: ActivityContext,
        task: QuestionTask,
    ) -> Result<BadgeAnswer, ActivityError> {
        let event = BadgeEvent {
            badge_id: self.badge_id.clone(),
            callsign: self.callsign.clone(),
            question_id: task.question.id.clone(),
            attempt: ctx.info().attempt,
        };
        if let Some(handle) = ctx.workflow_handle::<GameWorkflow>() {
            handle
                .signal(
                    GameWorkflow::badge_started,
                    event,
                    WorkflowSignalOptions::default(),
                )
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
        }

        tokio::time::sleep(self.answer_delay).await;
        let answer_number = self.answers_seen.fetch_add(1, Ordering::Relaxed);
        let correct = !(answer_number + self.answer_offset).is_multiple_of(5);
        let selected_index = if correct {
            task.question.correct_index
        } else {
            (task.question.correct_index + 1) % 4
        };
        println!(
            "{} answered {} {} on attempt {}",
            self.callsign,
            task.question.id,
            if correct { "correctly" } else { "wrong" },
            ctx.info().attempt
        );
        Ok(BadgeAnswer {
            badge_id: self.badge_id.clone(),
            callsign: self.callsign.clone(),
            question_id: task.question.id,
            selected_index,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let index = parse_badge_index()?;
    let client = connect_cloud().await?;
    let runtime = Runtime::new_assume_tokio(
        RuntimeOptions::builder()
            .heartbeat_interval(Some(Duration::from_secs(10)))
            .build()
            .map_err(|error| anyhow!(error))?,
    )?;
    let callsign = format!("SIM-{index:02}");
    let worker_identity = format!("badge/{callsign}");
    let mut tuner = TunerBuilder::default();
    tuner.activity_slot_supplier(Arc::new(FixedSizeSlotSupplier::<ActivitySlotKind>::new(1)));
    let worker_options = WorkerOptions::new(BADGE_TASK_QUEUE)
        .client_identity_override(worker_identity)
        .tuner(Arc::new(tuner.build()))
        .deployment_options(WorkerDeploymentOptions::from_build_id(
            "temporal-trivia-simulator-0.1.0".to_owned(),
        ))
        .register_activities(SimulatedBadge {
            badge_id: format!("sim-{index:02}"),
            callsign: callsign.clone(),
            answer_delay: Duration::from_millis(650 + (index as u64 % 5) * 125),
            answer_offset: index as u64,
            answers_seen: AtomicU64::new(0),
        })
        .build();
    let mut worker = Worker::new(&runtime, client, worker_options)
        .map_err(|error| anyhow!(error.to_string()))?;

    println!("{callsign} polling {BADGE_TASK_QUEUE}; stop with Ctrl-C");
    worker
        .run()
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(())
}

fn parse_badge_index() -> Result<usize> {
    let badge_index = std::env::args()
        .nth(1)
        .map(|value| {
            value
                .parse::<usize>()
                .context("badge index must be an integer")
        })
        .transpose()?
        .unwrap_or(1);
    if !(1..=MAX_BADGE_INDEX).contains(&badge_index) {
        bail!("badge index must be between 1 and {MAX_BADGE_INDEX}");
    }
    Ok(badge_index)
}

async fn connect_cloud() -> Result<Client> {
    let profile = cloud::load_profile()?;
    cloud::connect(&profile).await
}

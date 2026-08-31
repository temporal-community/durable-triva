use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use temporal_trivia_shared::{
    BadgeAnswer, BadgeEvent, GameInput, GameSnapshot, PHONE_TASK_QUEUE, PhoneActivityReady,
    PhoneJoin, QuestionTask, parse_env,
};
use temporalio_client::{
    Client, ClientOptions, Connection, ConnectionOptions, TlsOptions, WorkflowSignalOptions,
};
use temporalio_common::worker::WorkerDeploymentOptions;
use temporalio_macros::{activities, workflow, workflow_methods};
use temporalio_sdk::{
    Runtime, SyncWorkflowContext, Worker, WorkerOptions, WorkflowContext, WorkflowResult,
    activities::{ActivityContext, ActivityError},
    runtime::RuntimeOptions,
};
use temporalio_sdk_core::{ActivitySlotKind, FixedSizeSlotSupplier, TunerBuilder, Url};

const ACTIVE_WORKFLOW_ID: &str = "temporal-trivia-active";
const MAX_ACTIVITY_SLOTS: usize = 128;

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
        unreachable!("the phone dispatcher only registers Activity Workers")
    }

    #[signal]
    fn phone_joined(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _join: PhoneJoin) {}

    #[signal]
    fn phone_activity_ready(
        &mut self,
        _ctx: &mut SyncWorkflowContext<Self>,
        _ready: PhoneActivityReady,
    ) {
    }

    #[signal]
    fn phone_crashed(&mut self, _ctx: &mut SyncWorkflowContext<Self>, _event: BadgeEvent) {}
}

struct PhoneDispatcher;

#[activities]
impl PhoneDispatcher {
    #[activity(name = "trivia.answer_question")]
    #[allow(dead_code)]
    async fn answer_question(
        self: Arc<Self>,
        ctx: ActivityContext,
        task: QuestionTask,
    ) -> Result<BadgeAnswer, ActivityError> {
        let info = ctx.info();
        let activity_id = info.activity_id.clone();
        let workflow_run_id = info.workflow_run_id.clone().unwrap_or_default();
        let attempt = info.attempt;
        let Some(handle) = ctx.workflow_handle::<GameWorkflow>() else {
            return Err(anyhow!("phone Activity is not attached to a Workflow").into());
        };
        ctx.record_heartbeat(())
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        handle
            .signal(
                GameWorkflow::phone_activity_ready,
                PhoneActivityReady {
                    activity_id: activity_id.clone(),
                    workflow_run_id,
                    attempt,
                    task,
                },
                WorkflowSignalOptions::default(),
            )
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        println!("Queued {activity_id} attempt {attempt} for phone assignment");
        Err(ActivityError::WillCompleteAsync)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = connect_cloud().await?;
    let runtime = Runtime::new_assume_tokio(
        RuntimeOptions::builder()
            .heartbeat_interval(Some(Duration::from_secs(1)))
            .build()
            .map_err(|error| anyhow!(error))?,
    )?;
    let mut tuner = TunerBuilder::default();
    tuner.activity_slot_supplier(Arc::new(FixedSizeSlotSupplier::<ActivitySlotKind>::new(
        MAX_ACTIVITY_SLOTS,
    )));
    let worker_options = WorkerOptions::new(PHONE_TASK_QUEUE)
        .client_identity_override(format!("serverless-phone/{}", std::process::id()))
        .tuner(Arc::new(tuner.build()))
        .deployment_options(WorkerDeploymentOptions::from_build_id(
            "temporal-trivia-phone-basic-0.1.0".to_owned(),
        ))
        .register_activities(PhoneDispatcher)
        .build();
    let mut worker = Worker::new(&runtime, client, worker_options)
        .map_err(|error| anyhow!(error.to_string()))?;
    println!("Phone dispatcher polling {PHONE_TASK_QUEUE} for {ACTIVE_WORKFLOW_ID}");
    worker
        .run()
        .await
        .map_err(|error| anyhow!(error.to_string()))?;
    Ok(())
}

async fn connect_cloud() -> Result<Client> {
    let settings = read_cloud_settings()?;
    let address = required(&settings, "TEMPORAL_ADDRESS")?;
    let target = if address.contains("://") {
        address.to_owned()
    } else {
        format!("https://{address}")
    };
    let options = ConnectionOptions::new(Url::from_str(&target)?)
        .api_key(required(&settings, "TEMPORAL_API_KEY")?)
        .tls_options(TlsOptions::default())
        .build();
    let connection = Connection::connect(options).await?;
    Client::new(
        connection,
        ClientOptions::new(required(&settings, "TEMPORAL_NAMESPACE")?).build(),
    )
    .map_err(Into::into)
}

fn read_cloud_settings() -> Result<HashMap<String, String>> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project = manifest.parent().context("locate repository root")?;
    let local_path = project.join(".env");
    let fallback_path = project.join(".env.temporal");
    let path = std::env::var_os("TEMPORAL_ENV_FILE")
        .map(PathBuf::from)
        .or_else(|| local_path.is_file().then_some(local_path))
        .or_else(|| fallback_path.is_file().then_some(fallback_path));
    let mut settings = path
        .as_deref()
        .map(read_env_file)
        .transpose()?
        .unwrap_or_default();
    for key in ["TEMPORAL_ADDRESS", "TEMPORAL_NAMESPACE", "TEMPORAL_API_KEY"] {
        if let Ok(value) = std::env::var(key) {
            settings.insert(key.to_owned(), value);
        }
    }
    Ok(settings)
}

fn read_env_file(path: &Path) -> Result<HashMap<String, String>> {
    parse_env(&std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
        .map_err(|error| anyhow!("parse {}: {error}", path.display()))
}

fn required<'a>(settings: &'a HashMap<String, String>, name: &str) -> Result<&'a str> {
    let value = settings.get(name).map(String::as_str).unwrap_or("");
    if value.is_empty() {
        bail!("missing {name}; set it in the environment or .env.temporal");
    }
    Ok(value)
}

use std::sync::Mutex;

use anyhow::{Result, anyhow};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition, EspNvs};
use serde::{Deserialize, Serialize};

const SESSION_KEY: &str = "session";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedSession {
    pub game_id: String,
    pub deadline_unix_ms: u64,
    pub abandoned_questions: Vec<String>,
}

struct StoredSession {
    nvs: EspDefaultNvs,
    session: PersistedSession,
}

pub struct SessionStore {
    inner: Mutex<StoredSession>,
}

impl SessionStore {
    pub fn new(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspNvs::new(partition, "trivia", true)?;
        let session = load(&nvs)?;
        Ok(Self {
            inner: Mutex::new(StoredSession { nvs, session }),
        })
    }

    /// Records the round this badge is playing, extending the stored deadline
    /// if the Workflow granted the +30 second extension.
    pub fn begin_game(&self, game_id: &str, deadline_unix_ms: u64) -> Result<()> {
        let mut stored = self
            .inner
            .lock()
            .map_err(|_| anyhow!("session lock poisoned"))?;
        let is_new = stored.session.game_id != game_id;
        if is_new || stored.session.deadline_unix_ms < deadline_unix_ms {
            let next = if is_new {
                PersistedSession {
                    game_id: game_id.to_owned(),
                    deadline_unix_ms,
                    abandoned_questions: Vec::new(),
                }
            } else {
                PersistedSession {
                    deadline_unix_ms,
                    ..stored.session.clone()
                }
            };
            save(&stored.nvs, &next)?;
            stored.session = next;
        }
        Ok(())
    }

    pub fn is_abandoned(&self, game_id: &str, question_id: &str) -> Result<bool> {
        let stored = self
            .inner
            .lock()
            .map_err(|_| anyhow!("session lock poisoned"))?;
        Ok(stored.session.game_id == game_id
            && stored
                .session
                .abandoned_questions
                .iter()
                .any(|id| id == question_id))
    }

    pub fn abandon(&self, game_id: &str, question_id: &str) -> Result<()> {
        let mut stored = self
            .inner
            .lock()
            .map_err(|_| anyhow!("session lock poisoned"))?;
        if stored.session.game_id == game_id
            && !stored
                .session
                .abandoned_questions
                .iter()
                .any(|id| id == question_id)
        {
            let mut next = stored.session.clone();
            next.abandoned_questions.push(question_id.to_owned());
            save(&stored.nvs, &next)?;
            stored.session = next;
        }
        Ok(())
    }
}

fn load(nvs: &EspDefaultNvs) -> Result<PersistedSession> {
    let Some(length) = nvs.blob_len(SESSION_KEY)? else {
        return Ok(PersistedSession::default());
    };
    let mut buffer = vec![0_u8; length];
    let Some(bytes) = nvs.get_blob(SESSION_KEY, &mut buffer)? else {
        return Ok(PersistedSession::default());
    };
    match serde_json::from_slice(bytes) {
        Ok(session) => Ok(session),
        Err(error) => {
            log::warn!("discarding corrupt persisted trivia session: {error}");
            Ok(PersistedSession::default())
        }
    }
}

fn save(nvs: &EspDefaultNvs, session: &PersistedSession) -> Result<()> {
    let bytes = serde_json::to_vec(session)?;
    nvs.set_blob(SESSION_KEY, &bytes)?;
    Ok(())
}

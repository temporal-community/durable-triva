use std::{
    io::{ErrorKind, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::{input::ButtonReader, model::QuestionTask};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_COMMAND_BYTES: usize = 96;

/// Starts the USB-local hardware-in-the-loop command reader.
///
/// Compiled only under the non-default `hil` feature. `HIL ANSWER CORRECT`
/// reads the correct index out of the question this badge is holding, so a
/// shipped badge that carries this reader is a badge anyone with a USB cable
/// can win a round on. `tools/test_physical_badges.py` needs it; the badge
/// handed to an attendee must not have it.
///
/// Commands are accepted only while this physical badge owns a question:
/// `HIL ANSWER CORRECT` selects the question's known correct index, while
/// `HIL ANSWER 0` through `3` exercise an explicit directional mapping.
pub fn start(
    input: ButtonReader,
    activity_active: Arc<AtomicBool>,
    current_question: Arc<Mutex<Option<QuestionTask>>>,
    worker_polling: Arc<AtomicBool>,
    callsign: String,
) -> Result<()> {
    thread::Builder::new()
        .name("usb-hil".to_owned())
        // Formatting a log line through esp_log costs more than the 3 KiB
        // ESP-IDF gives a pthread by default, and a stack overflow here is a
        // reboot in the middle of an acceptance run.
        .stack_size(8 * 1024)
        .spawn(move || {
            read_commands(
                input,
                activity_active,
                current_question,
                worker_polling,
                callsign,
            )
        })
        .context("start USB HIL command reader")?;
    Ok(())
}

fn read_commands(
    input: ButtonReader,
    activity_active: Arc<AtomicBool>,
    current_question: Arc<Mutex<Option<QuestionTask>>>,
    worker_polling: Arc<AtomicBool>,
    callsign: String,
) {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut chunk = [0_u8; 64];
    let mut command = Vec::with_capacity(MAX_COMMAND_BYTES);

    loop {
        match stdin.read(&mut chunk) {
            Ok(0) => thread::sleep(POLL_INTERVAL),
            Ok(bytes_read) => {
                for byte in &chunk[..bytes_read] {
                    if *byte == b'\n' || *byte == b'\r' {
                        if !command.is_empty() {
                            if let Ok(line) = std::str::from_utf8(&command) {
                                handle_command(
                                    line.trim(),
                                    &input,
                                    &activity_active,
                                    &current_question,
                                    &worker_polling,
                                    &callsign,
                                );
                            } else {
                                log::warn!("HIL REJECT invalid-utf8");
                            }
                            command.clear();
                        }
                    } else if command.len() < MAX_COMMAND_BYTES {
                        command.push(*byte);
                    } else {
                        log::warn!("HIL REJECT command-too-long");
                        command.clear();
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => thread::sleep(POLL_INTERVAL),
            Err(error) => {
                log::warn!("HIL serial read failed: {error}");
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

fn handle_command(
    line: &str,
    input: &ButtonReader,
    activity_active: &Arc<AtomicBool>,
    current_question: &Arc<Mutex<Option<QuestionTask>>>,
    worker_polling: &Arc<AtomicBool>,
    callsign: &str,
) {
    if line == "HIL STATUS" {
        let question_id = current_question
            .lock()
            .ok()
            .and_then(|question| question.as_ref().map(|task| task.question.id.clone()))
            .unwrap_or_else(|| "none".to_owned());
        // `polling` is reported here because it is the only readiness signal a
        // runner can ask for. The boot log line it replaced is printed once,
        // so a port opened without resetting the badge never sees it.
        log::info!(
            "HIL STATUS callsign={} polling={} active={} question={}",
            callsign,
            worker_polling.load(Ordering::Acquire),
            activity_active.load(Ordering::Acquire),
            question_id
        );
        return;
    }

    let Some(argument) = line.strip_prefix("HIL ANSWER ") else {
        if line.starts_with("HIL") {
            log::warn!("HIL REJECT unknown-command");
        }
        return;
    };
    if !activity_active.load(Ordering::Acquire) {
        log::warn!("HIL REJECT no-active-question");
        return;
    }
    let Some(task) = current_question
        .lock()
        .ok()
        .and_then(|question| question.clone())
    else {
        log::warn!("HIL REJECT no-current-question");
        return;
    };
    let index = if argument == "CORRECT" {
        task.question.correct_index
    } else {
        match argument.parse::<u8>() {
            Ok(index @ 0..=3) => index,
            _ => {
                log::warn!("HIL REJECT answer-must-be-CORRECT-or-0-through-3");
                return;
            }
        }
    };
    let accepted = input.inject_answer(index);
    if accepted {
        log::info!(
            "HIL ACK answer={} question={} callsign={}",
            index,
            task.question.id,
            callsign
        );
    } else {
        log::warn!("HIL REJECT input-busy");
    }
}

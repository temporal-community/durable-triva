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

use crate::{input::BadgeInput, model::QuestionTask};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_COMMAND_BYTES: usize = 96;

/// Starts the USB-local hardware-in-the-loop command reader.
///
/// Commands are accepted only while this physical badge owns a question:
/// `HIL ANSWER CORRECT` selects the question's known correct index, while
/// `HIL ANSWER 0` through `3` exercise an explicit directional mapping.
pub fn start(
    input: Arc<Mutex<BadgeInput>>,
    activity_active: Arc<AtomicBool>,
    current_question: Arc<Mutex<Option<QuestionTask>>>,
    callsign: String,
) -> Result<()> {
    thread::Builder::new()
        .name("usb-hil".to_owned())
        .spawn(move || read_commands(input, activity_active, current_question, callsign))
        .context("start USB HIL command reader")?;
    Ok(())
}

fn read_commands(
    input: Arc<Mutex<BadgeInput>>,
    activity_active: Arc<AtomicBool>,
    current_question: Arc<Mutex<Option<QuestionTask>>>,
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
    input: &Arc<Mutex<BadgeInput>>,
    activity_active: &Arc<AtomicBool>,
    current_question: &Arc<Mutex<Option<QuestionTask>>>,
    callsign: &str,
) {
    if line == "HIL STATUS" {
        let question_id = current_question
            .lock()
            .ok()
            .and_then(|question| question.as_ref().map(|task| task.question.id.clone()))
            .unwrap_or_else(|| "none".to_owned());
        log::info!(
            "HIL STATUS callsign={} active={} question={}",
            callsign,
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
    let accepted = input
        .lock()
        .is_ok_and(|mut badge_input| badge_input.inject_answer(index));
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

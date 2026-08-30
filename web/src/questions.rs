use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use rand::{SeedableRng, rngs::StdRng, seq::SliceRandom};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::model::Question;

const SOURCE: &str = include_str!("../questions/source/open-trivia-db.json");
const MAX_PROMPT_CHARS: usize = 68;
const MAX_ANSWER_CHARS: usize = 20;

#[derive(Deserialize)]
struct SourceQuestion {
    #[serde(rename = "type")]
    kind: String,
    difficulty: String,
    category: String,
    question: String,
    correct_answer: String,
    incorrect_answers: Vec<String>,
}

pub fn build_deck(seed: u64, maximum: usize) -> Result<Vec<Question>> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut rust = rust_questions();
    let mut temporal = temporal_questions();
    let mut math = math_questions();
    let mut general = general_questions()?;
    validate_questions(rust.iter().chain(&temporal).chain(&math).chain(&general))?;
    for bucket in [&mut rust, &mut temporal, &mut math, &mut general] {
        bucket.shuffle(&mut rng);
    }

    let mut deck = Vec::with_capacity(maximum);
    let mut cursors = [0_usize; 4];
    let pattern = [(0, 30_usize), (1, 15_usize), (2, 15_usize), (3, 40_usize)];
    while deck.len() < maximum {
        let mut batch = Vec::with_capacity((maximum - deck.len()).min(100));
        for (bucket_index, count) in pattern {
            let bucket = match bucket_index {
                0 => &rust,
                1 => &temporal,
                2 => &math,
                _ => &general,
            };
            for _ in 0..count {
                if deck.len() + batch.len() == maximum || cursors[bucket_index] == bucket.len() {
                    break;
                }
                batch.push(bucket[cursors[bucket_index]].clone());
                cursors[bucket_index] += 1;
            }
        }
        if batch.is_empty() {
            break;
        }
        // Preserve the agreed category weights without presenting each
        // category as a contiguous run at the start of a round.
        batch.shuffle(&mut rng);
        deck.extend(batch);
    }

    // If a smaller authored category is exhausted during an unusually fast
    // round, preserve the no-duplicate and never-run-dry rules with general
    // questions instead of recycling technical prompts.
    while deck.len() < maximum && cursors[3] < general.len() {
        deck.push(general[cursors[3]].clone());
        cursors[3] += 1;
    }
    if deck.len() != maximum {
        bail!(
            "requested {maximum} questions, but only {} unique questions are available",
            deck.len()
        );
    }
    for question in &mut deck {
        shuffle_answers(question, &mut rng)?;
    }
    validate_questions(&deck)?;
    Ok(deck)
}

fn shuffle_answers(question: &mut Question, rng: &mut StdRng) -> Result<()> {
    let correct = question
        .answers
        .get(question.correct_index as usize)
        .cloned()
        .with_context(|| format!("invalid correct answer index for {}", question.id))?;
    question.answers.shuffle(rng);
    question.correct_index = question
        .answers
        .iter()
        .position(|answer| answer == &correct)
        .with_context(|| format!("correct answer disappeared while shuffling {}", question.id))?
        as u8;
    Ok(())
}

fn general_questions() -> Result<Vec<Question>> {
    let source: Vec<SourceQuestion> =
        serde_json::from_str(SOURCE).context("parse Open Trivia DB snapshot")?;
    let allowed_categories = [
        "Animals",
        "Art",
        "Entertainment: Board Games",
        "Entertainment: Books",
        "Entertainment: Film",
        "Entertainment: Music",
        "Entertainment: Television",
        "Entertainment: Video Games",
        "General Knowledge",
        "Geography",
        "History",
        "Science & Nature",
        "Science: Computers",
        "Science: Gadgets",
        "Science: Mathematics",
        "Vehicles",
    ];
    let blocked = [
        "porn",
        "hentai",
        "sexual",
        "suicide",
        "serial killer",
        "hitler",
        "nazi",
    ];
    let mut seen = HashSet::new();
    let mut accepted = Vec::new();
    for (index, raw) in source.into_iter().enumerate() {
        if raw.kind != "multiple"
            || raw.incorrect_answers.len() != 3
            || !allowed_categories.contains(&raw.category.as_str())
        {
            continue;
        }
        let prompt = decode(&raw.question);
        let correct = decode(&raw.correct_answer);
        let wrong: Vec<String> = raw
            .incorrect_answers
            .iter()
            .map(|item| decode(item))
            .collect();
        let combined = format!("{} {} {}", prompt, correct, wrong.join(" ")).to_lowercase();
        if blocked.iter().any(|word| combined.contains(word)) {
            continue;
        }
        let answers = [
            correct,
            wrong[0].clone(),
            wrong[1].clone(),
            wrong[2].clone(),
        ];
        if !fits(&prompt, &answers) || !seen.insert(prompt.to_lowercase()) {
            continue;
        }
        accepted.push(Question {
            id: stable_id("otdb", index, &prompt),
            category: "general".to_owned(),
            difficulty: raw.difficulty,
            prompt,
            answers,
            correct_index: 0,
        });
    }
    Ok(accepted)
}

fn decode(value: &str) -> String {
    html_escape::decode_html_entities(value).trim().to_owned()
}

fn stable_id(prefix: &str, index: usize, prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    format!(
        "{prefix}-{index}-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn fits(prompt: &str, answers: &[String; 4]) -> bool {
    prompt.chars().count() <= MAX_PROMPT_CHARS
        && answers
            .iter()
            .all(|answer| !answer.is_empty() && answer.chars().count() <= MAX_ANSWER_CHARS)
        && answers
            .iter()
            .map(|answer| answer.to_lowercase())
            .collect::<HashSet<_>>()
            .len()
            == 4
}

fn validate_questions<'a>(questions: impl IntoIterator<Item = &'a Question>) -> Result<()> {
    let mut ids = HashSet::new();
    let mut prompts = HashSet::new();
    for question in questions {
        if !fits(&question.prompt, &question.answers) {
            bail!("question does not fit badge: {}", question.id);
        }
        if question.correct_index > 3 {
            bail!("invalid correct answer index: {}", question.id);
        }
        if !ids.insert(&question.id) || !prompts.insert(question.prompt.to_lowercase()) {
            bail!("duplicate question in deck: {}", question.id);
        }
    }
    Ok(())
}

fn q(
    id: &str,
    category: &str,
    difficulty: &str,
    prompt: &str,
    correct: &str,
    wrong: [&str; 3],
) -> Question {
    Question {
        id: id.to_owned(),
        category: category.to_owned(),
        difficulty: difficulty.to_owned(),
        prompt: prompt.to_owned(),
        answers: [
            correct.to_owned(),
            wrong[0].to_owned(),
            wrong[1].to_owned(),
            wrong[2].to_owned(),
        ],
        correct_index: 0,
    }
}

fn rust_questions() -> Vec<Question> {
    vec![
        q(
            "rust-001",
            "rust",
            "easy",
            "Which tool builds a Rust package?",
            "Cargo",
            ["Clippy", "Rustdoc", "Miri"],
        ),
        q(
            "rust-002",
            "rust",
            "easy",
            "Which keyword creates an immutable binding?",
            "let",
            ["mut", "var", "const fn"],
        ),
        q(
            "rust-003",
            "rust",
            "easy",
            "Which keyword makes a binding mutable?",
            "mut",
            ["pub", "dyn", "move"],
        ),
        q(
            "rust-004",
            "rust",
            "easy",
            "What does Vec<T> store?",
            "Growable list",
            ["One value", "Key pairs", "Fixed tuple"],
        ),
        q(
            "rust-005",
            "rust",
            "easy",
            "Which type represents success or failure?",
            "Result",
            ["Option", "Future", "Iterator"],
        ),
        q(
            "rust-006",
            "rust",
            "easy",
            "Which type represents a value that may be absent?",
            "Option",
            ["Result", "Box", "String"],
        ),
        q(
            "rust-007",
            "rust",
            "easy",
            "Which Option variant contains a value?",
            "Some",
            ["Ok", "Value", "Present"],
        ),
        q(
            "rust-008",
            "rust",
            "easy",
            "Which Result variant contains an error?",
            "Err",
            ["None", "Fail", "Panic"],
        ),
        q(
            "rust-009",
            "rust",
            "easy",
            "What symbol creates a shared reference?",
            "&",
            ["*", "#", "@"],
        ),
        q(
            "rust-010",
            "rust",
            "easy",
            "Which macro prints a line to standard output?",
            "println!",
            ["format!", "dbg!", "write!"],
        ),
        q(
            "rust-011",
            "rust",
            "easy",
            "Which file declares a Cargo package?",
            "Cargo.toml",
            ["Cargo.lock", "rustfmt.toml", "main.rs"],
        ),
        q(
            "rust-012",
            "rust",
            "easy",
            "What is Rust's package unit called?",
            "Crate",
            ["Gem", "Pod", "Module file"],
        ),
        q(
            "rust-013",
            "rust",
            "medium",
            "Which keyword defines shared behavior?",
            "trait",
            ["impl", "where", "type"],
        ),
        q(
            "rust-014",
            "rust",
            "medium",
            "Which keyword implements methods for a type?",
            "impl",
            ["trait", "match", "use"],
        ),
        q(
            "rust-015",
            "rust",
            "medium",
            "Which construct exhaustively handles enum variants?",
            "match",
            ["loop", "if let", "where"],
        ),
        q(
            "rust-016",
            "rust",
            "medium",
            "What does the ? operator usually propagate?",
            "Errors",
            ["Threads", "Borrows", "Macros"],
        ),
        q(
            "rust-017",
            "rust",
            "medium",
            "Which smart pointer gives heap allocation?",
            "Box<T>",
            ["Rc<T>", "&mut T", "Cell<T>"],
        ),
        q(
            "rust-018",
            "rust",
            "medium",
            "Which type provides shared single-thread ownership?",
            "Rc<T>",
            ["Arc<T>", "Box<T>", "RefCell<T>"],
        ),
        q(
            "rust-019",
            "rust",
            "medium",
            "Which type provides atomic shared ownership?",
            "Arc<T>",
            ["Rc<T>", "Vec<T>", "Cow<T>"],
        ),
        q(
            "rust-020",
            "rust",
            "medium",
            "What does Send permit across threads?",
            "Value transfer",
            ["Shared mutation", "Async syntax", "Heap growth"],
        ),
        q(
            "rust-021",
            "rust",
            "medium",
            "What does Sync permit between threads?",
            "Shared access",
            ["Value moves", "Disk writes", "Panics"],
        ),
        q(
            "rust-022",
            "rust",
            "medium",
            "Which command runs Rust tests?",
            "cargo test",
            ["cargo bench", "cargo doc", "cargo tree"],
        ),
        q(
            "rust-023",
            "rust",
            "medium",
            "Which tool suggests idiomatic improvements?",
            "Clippy",
            ["Rustup", "Cargo", "Rustdoc"],
        ),
        q(
            "rust-024",
            "rust",
            "medium",
            "Which tool formats Rust source?",
            "rustfmt",
            ["rustdoc", "miri", "clippy"],
        ),
        q(
            "rust-025",
            "rust",
            "hard",
            "Which keyword creates a raw identifier?",
            "r#name",
            ["#name", "raw name", "@name"],
        ),
        q(
            "rust-026",
            "rust",
            "hard",
            "Which marker can opt a type out of Unpin?",
            "PhantomPinned",
            ["PhantomData", "Pin<Box>", "UnsafeCell"],
        ),
        q(
            "rust-027",
            "rust",
            "hard",
            "Which type enables checked interior mutation?",
            "RefCell<T>",
            ["Rc<T>", "Box<T>", "OnceLock<T>"],
        ),
        q(
            "rust-028",
            "rust",
            "hard",
            "What does 'static mean on a string literal?",
            "Program lifetime",
            ["Heap allocated", "Never borrowed", "Is mutable"],
        ),
        q(
            "rust-029",
            "rust",
            "hard",
            "Which trait powers .await?",
            "Future",
            ["Iterator", "Display", "Poll"],
        ),
        q(
            "rust-030",
            "rust",
            "hard",
            "Which enum does Future::poll return?",
            "Poll",
            ["Result", "ControlFlow", "PendingOnly"],
        ),
    ]
}

fn temporal_questions() -> Vec<Question> {
    vec![
        q(
            "temporal-001",
            "temporal",
            "easy",
            "What stores durable Workflow state?",
            "Event History",
            ["Worker RAM", "Task Queue", "Activity log"],
        ),
        q(
            "temporal-002",
            "temporal",
            "easy",
            "Who executes Workflow and Activity code?",
            "Workers",
            ["Namespaces", "Schedules", "Search attrs"],
        ),
        q(
            "temporal-003",
            "temporal",
            "easy",
            "What routes tasks to Workers?",
            "Task Queue",
            ["Signal", "Namespace", "Memo"],
        ),
        q(
            "temporal-004",
            "temporal",
            "easy",
            "What should contain failure-prone I/O?",
            "Activities",
            ["Queries", "Workflows", "Timers"],
        ),
        q(
            "temporal-005",
            "temporal",
            "easy",
            "What sends an asynchronous message to a Workflow?",
            "Signal",
            ["Query", "Heartbeat", "Memo"],
        ),
        q(
            "temporal-006",
            "temporal",
            "easy",
            "What reads Workflow state without changing it?",
            "Query",
            ["Signal", "Activity", "Update"],
        ),
        q(
            "temporal-007",
            "temporal",
            "easy",
            "What bounds an Activity attempt?",
            "Start-to-close",
            ["Workflow ID", "Task Queue", "Search attr"],
        ),
        q(
            "temporal-008",
            "temporal",
            "easy",
            "What detects failure in a long Activity quickly?",
            "Heartbeats",
            ["Queries", "Schedules", "Memos"],
        ),
        q(
            "temporal-009",
            "temporal",
            "medium",
            "What must Workflow code be during replay?",
            "Deterministic",
            ["Multithreaded", "Stateless", "Synchronous"],
        ),
        q(
            "temporal-010",
            "temporal",
            "medium",
            "What uniquely names a Workflow Execution series?",
            "Workflow ID",
            ["Task token", "Build ID", "Activity ID"],
        ),
        q(
            "temporal-011",
            "temporal",
            "medium",
            "What can safely retry after Worker failure?",
            "Activities",
            ["Queries", "Memos", "Search attrs"],
        ),
        q(
            "temporal-012",
            "temporal",
            "medium",
            "What starts fresh History while keeping the Workflow?",
            "Continue-As-New",
            ["Reset", "Heartbeat", "Signal"],
        ),
        q(
            "temporal-013",
            "temporal",
            "hard",
            "Which timeout includes every Activity retry?",
            "Schedule-to-close",
            ["Start-to-close", "Heartbeat", "Run timeout"],
        ),
        q(
            "temporal-014",
            "temporal",
            "hard",
            "What records Activity progress for the next attempt?",
            "Heartbeat detail",
            ["Query result", "Search attr", "Worker cache"],
        ),
        q(
            "temporal-015",
            "temporal",
            "hard",
            "What replays History to restore Workflow state?",
            "Workflow Worker",
            ["Activity Worker", "Cloud UI", "Task Queue"],
        ),
    ]
}

fn math_questions() -> Vec<Question> {
    let mut questions = Vec::new();
    for n in 11_i32..=110 {
        let left = n % 17 + 3;
        let right = n % 13 + 4;
        let answer = left + right;
        questions.push(q(
            &format!("math-add-{n:03}"),
            "math",
            if n < 50 { "easy" } else { "medium" },
            &format!("What is {left} + {right}?"),
            &answer.to_string(),
            [
                &(answer + 1).to_string(),
                &(answer - 1).to_string(),
                &(answer + 3).to_string(),
            ],
        ));
    }
    questions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_hundred_matches_agreed_mix_and_fits() {
        let deck = build_deck(7, 100).unwrap();
        assert_eq!(deck.len(), 100);
        assert_eq!(deck.iter().filter(|q| q.category == "rust").count(), 30);
        assert_eq!(deck.iter().filter(|q| q.category == "temporal").count(), 15);
        assert_eq!(deck.iter().filter(|q| q.category == "math").count(), 15);
        assert_eq!(deck.iter().filter(|q| q.category == "general").count(), 40);
    }

    #[test]
    fn opening_questions_mix_categories() {
        let deck = build_deck(7, 100).unwrap();
        let opening_categories: HashSet<_> = deck
            .iter()
            .take(10)
            .map(|question| question.category.as_str())
            .collect();

        assert!(
            opening_categories.len() >= 3,
            "opening questions should not be a single-category run"
        );
    }

    #[test]
    fn source_filter_has_large_fallback_pool() {
        assert!(general_questions().unwrap().len() > 300);
    }

    /// The board renders question text with real fonts, but the badge has a
    /// 3x5 bitmap face. A character it cannot draw shows as a question mark, so
    /// an unrenderable deck reads as gibberish on the hardware.
    #[test]
    fn every_shipped_question_is_renderable_on_the_badge() {
        let deck = build_deck(7, 400).expect("build a deck");
        let mut offenders = Vec::new();
        for question in &deck {
            let mut texts = vec![question.prompt.clone()];
            texts.extend(question.answers.iter().cloned());
            for text in texts {
                for character in text.chars() {
                    if !badge_screen::is_renderable(character) {
                        offenders.push((character, text.clone()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "{} characters cannot be drawn on the badge, e.g. {:?}",
            offenders.len(),
            &offenders[..offenders.len().min(5)]
        );
    }

    #[test]
    fn oversized_deck_request_fails_instead_of_returning_partial_work() {
        let error = build_deck(7, 10_000).unwrap_err();
        assert!(error.to_string().contains("unique questions are available"));
    }

    #[test]
    fn the_shipped_deck_stays_inside_the_payload_budget() {
        // Review finding W2. `main.rs` ships 500 questions as GameInput, which
        // lands in WorkflowExecutionStarted and stays in History for the life
        // of the execution. The server warns on blobs over 512 KB.
        let deck = crate::questions::build_deck(11, 500).expect("build the shipped deck");
        let input = crate::model::GameInput {
            game_id: "round-under-test".to_owned(),
            questions: deck,
            duration_seconds: crate::model::GAME_SECONDS,
            backlog_override: None,
            detected_badge_count: Some(10),
            index_search_attributes: true,
        };
        let bytes = serde_json::to_vec(&input)
            .expect("serialize GameInput")
            .len();
        assert!(
            bytes < 512 * 1024,
            "GameInput is {bytes} bytes, over the 512 KB blob warning"
        );
    }
}

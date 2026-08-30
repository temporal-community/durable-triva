//! Renders every badge screen to an HTML contact sheet so the 128x64 layout
//! can be reviewed without flashing hardware.
//!
//! Usage: `cargo run -p badge-screen --bin preview --target <host> > sheet.html`

use std::collections::BTreeMap;
use std::fmt::Write as _;

use badge_screen::{Canvas, HEIGHT, Status, WIDTH};
use temporal_trivia_shared::{ChaosCommand, GameSnapshot, GameStatus, PlayerScore, Question};

const SCALE: usize = 3;
const CALLSIGN: &str = "KEEN-RAVEN-C8";

fn question() -> Question {
    Question {
        id: "otdb-1163".to_owned(),
        category: "general".to_owned(),
        difficulty: "easy".to_owned(),
        // Punctuation that used to render as question marks, plus an unbroken
        // token longer than a panel line.
        prompt: "In HTML, which non-standard tag made elements scroll?".to_owned(),
        answers: [
            "<marquee></marquee>".to_owned(),
            "<scroll></scroll>".to_owned(),
            "Vec<T> & Rc<T>".to_owned(),
            "Never (cancelled)".to_owned(),
        ],
        correct_index: 0,
    }
}

fn snapshot(winners: &[&str]) -> GameSnapshot {
    let mut players = BTreeMap::new();
    for (index, callsign) in ["KEEN-RAVEN-C8", "TIDY-FALCON-A2", "SPRY-LEMUR-77"]
        .iter()
        .enumerate()
    {
        players.insert(
            format!("badge-{index}"),
            PlayerScore {
                badge_id: format!("badge-{index}"),
                callsign: (*callsign).to_owned(),
                score: 36 - (index as i32) * 4,
                correct: 48 - (index as u32) * 5,
                wrong: 12,
                panics: 0,
            },
        );
    }
    GameSnapshot {
        game_id: Some("trivia-7701e0a6".to_owned()),
        status: GameStatus::Finished,
        players,
        winners: winners.iter().map(|name| (*name).to_owned()).collect(),
        ..Default::default()
    }
}

fn screens() -> Vec<(String, Canvas)> {
    let mut out: Vec<(String, Canvas)> = Vec::new();
    let mut push = |label: &str, draw: &dyn Fn(&mut Canvas)| {
        let mut canvas = Canvas::new();
        draw(&mut canvas);
        out.push((label.to_owned(), canvas));
    };

    for status in Status::ALL {
        push(
            &format!("status: {}", status.headline()),
            &|canvas: &mut Canvas| {
                canvas.status(CALLSIGN, status);
            },
        );
    }
    push("waiting (idle worker)", &|canvas| canvas.waiting(CALLSIGN));
    push("question", &|canvas| canvas.question(CALLSIGN, &question()));
    push("feedback: correct +1", &|canvas| {
        canvas.feedback(CALLSIGN, true, 1)
    });
    push("feedback: correct +2 (double points)", &|canvas| {
        canvas.feedback(CALLSIGN, true, 2)
    });
    push("feedback: wrong -2 (double points)", &|canvas| {
        canvas.feedback(CALLSIGN, false, -2)
    });
    push("panic (worker crash)", &|canvas| canvas.panic(CALLSIGN));
    push("recovered", &|canvas| canvas.recovered(CALLSIGN));
    for command in [
        ChaosCommand::DoublePoints,
        ChaosCommand::RustOnly,
        ChaosCommand::SuddenDeath,
        ChaosCommand::ExtendThirtySeconds,
    ] {
        push(&format!("powerup: {command:?}"), &|canvas: &mut Canvas| {
            canvas.powerup(CALLSIGN, command)
        });
    }
    push("sleep countdown", &|canvas| {
        canvas.sleep_countdown(CALLSIGN, 2)
    });
    push("sleeping", &|canvas| canvas.sleeping(CALLSIGN));
    push("results: won", &|canvas| {
        canvas.results(CALLSIGN, "badge-0", &snapshot(&["KEEN-RAVEN-C8"]))
    });
    push("results: lost", &|canvas| {
        canvas.results(CALLSIGN, "badge-2", &snapshot(&["KEEN-RAVEN-C8"]))
    });
    push("results: three-way tie", &|canvas| {
        canvas.results(
            CALLSIGN,
            "badge-0",
            &snapshot(&["KEEN-RAVEN-C8", "TIDY-FALCON-A2", "SPRY-LEMUR-77"]),
        )
    });
    out
}

fn render(canvas: &Canvas) -> String {
    // One box-shadow per lit pixel keeps the sheet dependency-free.
    let mut shadows = Vec::new();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            if canvas.is_lit(x, y) {
                shadows.push(format!("{}px {}px 0 0 #cbb484", x * SCALE, y * SCALE));
            }
        }
    }
    format!(
        "<div class=panel><i style=\"box-shadow:{}\"></i></div>",
        if shadows.is_empty() {
            "none".to_owned()
        } else {
            shadows.join(",")
        }
    )
}

fn main() {
    let mut html = String::new();
    html.push_str(
        "<!doctype html><meta charset=utf-8><title>Badge screens</title><style>\
body{margin:0;padding:14px;background:#080a09;color:#7f858c;\
font:11px ui-monospace,monospace;display:grid;\
grid-template-columns:repeat(auto-fill,minmax(400px,1fr));gap:14px}\
figure{margin:0}\
figcaption{padding:4px 0;letter-spacing:.08em;text-transform:uppercase}\
.panel{position:relative;width:384px;height:192px;background:#05070a;\
border:1px solid #1d2a24;border-radius:3px}\
.panel i{position:absolute;top:0;left:0;width:3px;height:3px}\
</style>",
    );
    for (label, canvas) in screens() {
        let _ = write!(
            html,
            "<figure><figcaption>{label}</figcaption>{}</figure>",
            render(&canvas)
        );
    }
    println!("{html}");
}

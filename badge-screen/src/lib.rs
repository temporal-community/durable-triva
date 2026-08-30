//! The badge's OLED screens, as pure framebuffer composition.
//!
//! This crate deliberately has no ESP-IDF dependency. The firmware owns the
//! I2C transport and nothing else, so every screen the badge can draw is unit
//! testable and previewable from a development host.

use std::fmt;

use temporal_trivia_shared::{ChaosCommand, GameSnapshot, Question};

/// Panel width in pixels. The SSD1306 the badge carries is 128x64.
pub const WIDTH: usize = 128;
/// Panel height in pixels.
pub const HEIGHT: usize = 64;
/// Framebuffer size in bytes: one bit per pixel, eight rows to a page.
pub const BUFFER_LEN: usize = WIDTH * HEIGHT / 8;

/// Full-size glyphs are 5x7 drawn on a 6px pitch.
const GLYPH_WIDTH: usize = 5;
const GLYPH_ADVANCE: usize = 6;
/// Compact glyphs are the same source downsampled to 3x5 on a 4px pitch.
const COMPACT_ADVANCE: usize = 4;
const COMPACT_WIDTH: usize = 3;
const COMPACT_HEIGHT: usize = 5;

const HEADER_RULE_Y: usize = 8;
const HEADLINE_Y: usize = 11;
const DETAIL_Y: usize = 22;
/// First row the instruction block may occupy, just below the detail line.
const INSTRUCTION_TOP: usize = 28;
const INSTRUCTION_PITCH: usize = 7;
const MAX_INSTRUCTIONS: usize = 4;

/// Longest run of full-size glyphs that fits the panel width.
const MAX_HEADLINE_CHARS: usize = WIDTH / GLYPH_ADVANCE;
/// Longest run of compact glyphs that fits the panel width.
const MAX_COMPACT_CHARS: usize = WIDTH / COMPACT_ADVANCE - 1;

const ANSWER_PANEL_WIDTH: usize = 63;
const ANSWER_PANEL_HEIGHT: usize = 16;
const ANSWER_LABEL_CHARS: usize = 11;
const ANSWER_LABEL_LINES: usize = 2;
const PROMPT_CHARS: usize = 31;
const PROMPT_LINES: usize = 3;

/// A boot-sequence screen.
///
/// Headline and instruction travel together: they used to be a `&str` headline
/// and a `match` on its text, so a typo silently fell through to "PLEASE WAIT"
/// instead of failing to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Booting,
    ConnectingWifi,
    SyncingTime,
    ConnectingCloud,
    ResultPending,
}

impl Status {
    /// Every variant, in the order the badge reaches them. `preview` renders
    /// this, so a new variant cannot be added without a picture of it.
    pub const ALL: [Self; 5] = [
        Self::Booting,
        Self::ConnectingWifi,
        Self::SyncingTime,
        Self::ConnectingCloud,
        Self::ResultPending,
    ];

    #[must_use]
    pub const fn headline(self) -> &'static str {
        match self {
            Self::Booting => "BOOTING",
            Self::ConnectingWifi => "CONNECTING WIFI",
            Self::SyncingTime => "SYNCING TIME",
            Self::ConnectingCloud => "CONNECTING CLOUD",
            Self::ResultPending => "RESULT PENDING",
        }
    }

    #[must_use]
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::Booting => "STARTING RUST WORKER",
            Self::ConnectingWifi => "JOINING BADGE NETWORK",
            Self::SyncingTime => "PREPARING CLOUD TLS",
            Self::ConnectingCloud => "CONNECTING TEMPORAL",
            Self::ResultPending => "TEMPORAL HAS THE RESULT",
        }
    }
}

/// A 1-bit 128x64 framebuffer laid out the way the SSD1306 expects it.
#[derive(Clone, PartialEq, Eq)]
pub struct Canvas {
    buffer: [u8; BUFFER_LEN],
}

/// Reports the shape and how much of it is lit, not the buffer.
///
/// A derived `Debug` would print all 1024 bytes into every failing
/// `assert_eq!`, which buries the assertion that failed.
impl fmt::Debug for Canvas {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lit: u32 = self.buffer.iter().map(|byte| byte.count_ones()).sum();
        formatter
            .debug_struct("Canvas")
            .field("width", &WIDTH)
            .field("height", &HEIGHT)
            .field("lit_pixels", &lit)
            .finish()
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas {
    pub const fn new() -> Self {
        Self {
            buffer: [0; BUFFER_LEN],
        }
    }

    /// Raw framebuffer, ready to be pushed to the panel.
    /// The raw framebuffer, ready to hand to the SSD1306 unaltered.
    pub fn bits(&self) -> &[u8; BUFFER_LEN] {
        &self.buffer
    }

    /// Blanks every pixel. Each screen method calls this first, so callers
    /// only need it when composing a frame by hand.
    pub fn clear(&mut self) {
        self.buffer.fill(0);
    }

    /// Whether one pixel is lit. Out-of-range coordinates read as unlit
    /// rather than panicking, which keeps assertions in tests total.
    pub fn is_lit(&self, x: usize, y: usize) -> bool {
        if x >= WIDTH || y >= HEIGHT {
            return false;
        }
        self.buffer[x + (y / 8) * WIDTH] & (1 << (y % 8)) != 0
    }

    fn set_pixel(&mut self, x: usize, y: usize) {
        if x < WIDTH && y < HEIGHT {
            self.buffer[x + (y / 8) * WIDTH] |= 1 << (y % 8);
        }
    }

    // ---------- screens ----------

    /// A boot-sequence screen: headline plus its fixed instruction line.
    pub fn status(&mut self, callsign: &str, status: Status) {
        self.centered(callsign, status.headline(), "", &[status.instruction()]);
    }

    /// The idle screen shown while this badge's Worker has no Activity.
    pub fn waiting(&mut self, callsign: &str) {
        self.centered(
            callsign,
            "POLLING TEMPORAL",
            "NEXT QUESTION AUTO",
            &[
                "ANSWER: PRESS DIRECTION",
                "CRASH: HOLD LEFT+RIGHT",
                "SLEEP: HOLD DOWN 3 SEC",
            ],
        );
    }

    /// The overlay announcing an operator power-up that landed as a
    /// validated Workflow Update.
    pub fn powerup(&mut self, callsign: &str, command: ChaosCommand) {
        let (headline, detail, instruction) = match command {
            ChaosCommand::DoublePoints => ("DOUBLE POINTS", "SCORES X2", "TEMPORAL UPDATE APPLIED"),
            ChaosCommand::RustOnly => ("RUST ONLY", "10 SECOND FILTER", "TEMPORAL UPDATE APPLIED"),
            ChaosCommand::SuddenDeath => {
                ("SUDDEN DEATH", "NEXT RIGHT WINS", "TEMPORAL UPDATE APPLIED")
            }
            ChaosCommand::ExtendThirtySeconds => {
                ("TIME EXTENDED", "+30 SECONDS", "DURABLE TIMER UPDATED")
            }
        };
        self.centered(callsign, headline, detail, &[instruction]);
    }

    /// Counts the badge down to deep sleep.
    pub fn sleep_countdown(&mut self, callsign: &str, seconds: u64) {
        self.centered(
            callsign,
            &format!("SLEEP IN {seconds}"),
            "POWERING DOWN",
            &["KEEP HOLDING DOWN", "RELEASE TO CANCEL"],
        );
    }

    /// The last frame drawn before deep sleep; it stays on the panel
    /// because the SSD1306 holds its contents without a Worker.
    pub fn sleeping(&mut self, callsign: &str) {
        self.centered(
            callsign,
            "SLEEPING",
            "POWER OFF ARMED",
            &["RELEASE BUTTON", "ANY BUTTON WAKES"],
        );
    }

    /// A question with its four answers on the button positions.
    pub fn question(&mut self, callsign: &str, question: &Question) {
        self.clear();
        self.header(callsign);
        self.compact_wrapped(1, HEADLINE_Y, &question.prompt, PROMPT_CHARS, PROMPT_LINES);

        // Same physical layout as the badge's button cluster: top/right on row
        // one, left/down on row two.
        for (index, &(x, y)) in [(0, 30), (65, 30), (0, 48), (65, 48)].iter().enumerate() {
            self.answer_panel(index, x, y, &question.answers[index]);
        }
    }

    /// `score_delta` is the value Temporal will actually record, so a badge
    /// under double points agrees with the board instead of always claiming 1.
    /// The verdict screen. `score_delta` is what Temporal will record,
    /// already signed and already doubled if double points was in force.
    pub fn feedback(&mut self, callsign: &str, correct: bool, score_delta: i32) {
        let score = format!("SCORE {score_delta:+}");
        self.centered(
            callsign,
            if correct { "CORRECT" } else { "WRONG" },
            &score,
            &[
                if correct {
                    "ANSWER ACCEPTED"
                } else {
                    "WRONG ANSWER"
                },
                if correct {
                    "TEMPORAL RECORDED IT"
                } else {
                    "ACTIVITY COMPLETED"
                },
            ],
        );
    }

    /// The simulated-crash screen, shown while the badge withholds
    /// heartbeats so Temporal reassigns its question.
    pub fn panic(&mut self, callsign: &str) {
        self.centered(
            callsign,
            "WORKER CRASH",
            "TEMPORAL HOLDS TASK",
            &["HEARTBEATS STOPPED", "QUESTION WILL RETRY"],
        );
    }

    /// Shown once the heartbeat blackout ends and the question has moved on.
    pub fn recovered(&mut self, callsign: &str) {
        self.centered(
            callsign,
            "WORKER BACK",
            "QUESTION RETURNED",
            &["CONNECTED TO TEMPORAL", "READY FOR NEW QUESTION"],
        );
    }

    /// Final standings, with this badge's own row called out.
    pub fn results(&mut self, callsign: &str, badge_id: &str, snapshot: &GameSnapshot) {
        let own = snapshot.players.get(badge_id);
        let own_score = own.map(|player| player.score).unwrap_or(0);
        let place = 1 + snapshot
            .players
            .values()
            .filter(|player| player.score > own_score)
            .count();
        let won = own.is_some_and(|player| snapshot.winners.contains(&player.callsign));
        let correct = own.map(|player| player.correct).unwrap_or(0);
        let wrong = own.map(|player| player.wrong).unwrap_or(0);
        let score_label = format!("SCORE {own_score}");
        let place_label = format!("PLACE {place}");
        let answer_label = format!("RIGHT {correct} / WRONG {wrong}");
        self.centered(
            callsign,
            if won { "YOU WON" } else { "ROUND OVER" },
            &winner_line(&snapshot.winners),
            &[&score_label, &place_label, &answer_label],
        );
    }

    // ---------- layout ----------

    fn header(&mut self, callsign: &str) {
        self.text(0, 0, callsign);
        self.hline(0, WIDTH - 1, HEADER_RULE_Y);
    }

    fn centered(&mut self, callsign: &str, headline: &str, detail: &str, instructions: &[&str]) {
        self.clear();
        self.header(callsign);
        self.centered_text(HEADLINE_Y, headline);
        self.centered_compact(DETAIL_Y, detail);
        let shown = instructions.len().min(MAX_INSTRUCTIONS);
        for (index, instruction) in instructions.iter().take(MAX_INSTRUCTIONS).enumerate() {
            self.centered_compact(instruction_y(shown, index), instruction);
        }
    }

    // ---------- text ----------

    fn text(&mut self, mut x: usize, y: usize, text: &str) {
        for character in text.chars() {
            if x + GLYPH_WIDTH > WIDTH {
                break;
            }
            self.draw_glyph(x, y, character);
            x += GLYPH_ADVANCE;
        }
    }

    fn centered_text(&mut self, y: usize, text: &str) {
        let visible: String = text.chars().take(MAX_HEADLINE_CHARS).collect();
        let width = visible
            .chars()
            .count()
            .saturating_mul(GLYPH_ADVANCE)
            .saturating_sub(1);
        self.text((WIDTH.saturating_sub(width)) / 2, y, &visible);
    }

    fn centered_compact(&mut self, y: usize, text: &str) {
        let visible: String = text.chars().take(MAX_COMPACT_CHARS).collect();
        let width = visible
            .chars()
            .count()
            .saturating_mul(COMPACT_ADVANCE)
            .saturating_sub(1);
        self.compact_text((WIDTH.saturating_sub(width)) / 2, y, &visible);
    }

    fn compact_text(&mut self, mut x: usize, y: usize, text: &str) {
        for character in text.chars() {
            if x + COMPACT_WIDTH > WIDTH {
                break;
            }
            self.draw_compact_glyph(x, y, character);
            x += COMPACT_ADVANCE;
        }
    }

    fn compact_wrapped(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        max_chars: usize,
        max_lines: usize,
    ) {
        for (index, line) in wrap(text, max_chars, max_lines).iter().enumerate() {
            self.compact_text(x, y + index * (COMPACT_HEIGHT + 1), line);
        }
    }

    fn draw_glyph(&mut self, x: usize, y: usize, character: char) {
        for (column, bits) in glyph(character).iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    self.set_pixel(x + column, y + row);
                }
            }
        }
    }

    /// Draws from the real 3x5 font. Downsampling the 5x7 face mechanically
    /// used to collapse N onto H and 0 onto 8, so most small text on the badge
    /// was misspelled.
    fn draw_compact_glyph(&mut self, x: usize, y: usize, character: char) {
        for (row, bits) in compact_glyph(character).iter().enumerate() {
            for column in 0..COMPACT_WIDTH {
                if bits & (1 << column) != 0 {
                    self.set_pixel(x + column, y + row);
                }
            }
        }
    }

    fn hline(&mut self, from_x: usize, to_x: usize, y: usize) {
        for x in from_x..=to_x {
            self.set_pixel(x, y);
        }
    }

    fn frame(&mut self, x: usize, y: usize, width: usize, height: usize) {
        for column in (x + 2)..(x + width - 2) {
            self.set_pixel(column, y);
            self.set_pixel(column, y + height - 1);
        }
        for row in (y + 2)..(y + height - 2) {
            self.set_pixel(x, row);
            self.set_pixel(x + width - 1, row);
        }
        for (dx, dy) in [
            (1, 1),
            (width - 2, 1),
            (1, height - 2),
            (width - 2, height - 2),
        ] {
            self.set_pixel(x + dx, y + dy);
        }
    }

    fn answer_panel(&mut self, index: usize, x: usize, y: usize, label: &str) {
        self.frame(x, y, ANSWER_PANEL_WIDTH, ANSWER_PANEL_HEIGHT);
        self.button_glyph(index, x + 2, y + 3);
        self.compact_wrapped(x + 14, y + 3, label, ANSWER_LABEL_CHARS, ANSWER_LABEL_LINES);
    }

    fn button_glyph(&mut self, answer_index: usize, x: usize, y: usize) {
        let bits = match answer_index {
            0 => &BUTTON_TOP,
            1 => &BUTTON_RIGHT,
            2 => &BUTTON_LEFT,
            _ => &BUTTON_DOWN,
        };
        for row in 0..10 {
            let row_bits = u16::from(bits[row * 2]) | (u16::from(bits[row * 2 + 1]) << 8);
            for column in 0..10 {
                if row_bits & (1 << column) != 0 {
                    self.set_pixel(x + column, y + row);
                }
            }
        }
    }
}

/// Centres the instruction block in the space under the detail line. The old
/// hand-tuned table produced exactly these offsets for two, three and four
/// instructions, so this is that intent expressed as arithmetic.
fn instruction_y(count: usize, index: usize) -> usize {
    let block = count.saturating_mul(INSTRUCTION_PITCH).saturating_sub(2);
    let space = HEIGHT - INSTRUCTION_TOP;
    let top = INSTRUCTION_TOP + space.saturating_sub(block) / 2;
    top + index * INSTRUCTION_PITCH
}

/// A tie can name more badges than the panel can show, so name the first and
/// count the rest rather than truncating mid-callsign.
fn winner_line(winners: &[String]) -> String {
    match winners.split_first() {
        None => "NO WINNER".to_owned(),
        Some((first, [])) => format!("WINNER {first}"),
        Some((first, rest)) => format!("WINNER {first} +{} TIED", rest.len()),
    }
}

fn wrap(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        // A token with no spaces in it -- `<marquee></marquee>`, `Vec<T>` -- can
        // be longer than the line budget. Break it across lines instead of
        // silently dropping the tail.
        let mut remaining = word;
        loop {
            let remaining_len = remaining.chars().count();
            let needed = current.chars().count() + usize::from(!current.is_empty()) + remaining_len;
            if needed > max_chars && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                if lines.len() == max_lines {
                    return lines;
                }
                continue;
            }
            if remaining_len > max_chars {
                let split = remaining
                    .char_indices()
                    .nth(max_chars)
                    .map_or(remaining.len(), |(index, _)| index);
                lines.push(remaining[..split].to_owned());
                if lines.len() == max_lines {
                    return lines;
                }
                remaining = &remaining[split..];
                continue;
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(remaining);
            break;
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    lines
}

const BUTTON_TOP: [u8; 20] = [
    0x30, 0x00, 0x78, 0x00, 0x78, 0x00, 0xB6, 0x01, 0x49, 0x02, 0x49, 0x02, 0xB6, 0x01, 0x48, 0x00,
    0x48, 0x00, 0x30, 0x00,
];
const BUTTON_RIGHT: [u8; 20] = [
    0x30, 0x00, 0x48, 0x00, 0x48, 0x00, 0xB6, 0x01, 0xC9, 0x03, 0xC9, 0x03, 0xB6, 0x01, 0x48, 0x00,
    0x48, 0x00, 0x30, 0x00,
];
const BUTTON_DOWN: [u8; 20] = [
    0x30, 0x00, 0x48, 0x00, 0x48, 0x00, 0xB6, 0x01, 0x49, 0x02, 0x49, 0x02, 0xB6, 0x01, 0x78, 0x00,
    0x78, 0x00, 0x30, 0x00,
];
const BUTTON_LEFT: [u8; 20] = [
    0x30, 0x00, 0x48, 0x00, 0x48, 0x00, 0xB6, 0x01, 0x4F, 0x02, 0x4F, 0x02, 0xB6, 0x01, 0x48, 0x00,
    0x48, 0x00, 0x30, 0x00,
];

/// Folds the characters a real trivia deck contains onto the ASCII the font
/// draws: accented Latin letters lose their accents, and typographic
/// punctuation becomes its ASCII equivalent.
fn fold(character: char) -> char {
    match character {
        'À'..='Å' | 'à'..='å' => 'A',
        'Ç' | 'ç' => 'C',
        'È'..='Ë' | 'è'..='ë' => 'E',
        'Ì'..='Ï' | 'ì'..='ï' => 'I',
        'Ñ' | 'ñ' => 'N',
        'Ò'..='Ö' | 'Ø' | 'ò'..='ö' | 'ø' => 'O',
        'Ù'..='Ü' | 'ù'..='ü' => 'U',
        'Ý' | 'ý' | 'ÿ' => 'Y',
        'ß' => 'S',
        'Æ' | 'æ' => 'A',
        '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{00B4}' | '`' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{00AB}' | '\u{00BB}' => '"',
        '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
        '\u{2026}' => '.',
        '\u{00A0}' | '\u{2007}' | '\u{202F}' => ' ',
        '\u{00D7}' => 'X',
        other => other.to_ascii_uppercase(),
    }
}

/// True when the font has a real glyph for this character rather than falling
/// back. Used by the deck test so unrenderable questions cannot ship.
/// Whether the compact face can draw this character.
///
/// `questions.rs` filters the deck through this, so a question that would
/// render as blanks on the panel never reaches a badge.
pub fn is_renderable(character: char) -> bool {
    let folded = fold(character);
    folded.is_ascii_alphanumeric() || KNOWN_PUNCTUATION.contains(folded)
}

const KNOWN_PUNCTUATION: &str = " -_.:/+,'\"()&!?;=<>%#*@|$[]{}^~\\";

/// Tom Thumb (also published as Fixed4x6): a 3x5 monospace bitmap face by
/// Brian Swetland with readability revisions by Robey Pointer, released for any
/// use under CC0 / CC-BY 3.0. Rows run top to bottom; bit 0 is the leftmost
/// column.
const COMPACT_FONT: [[u8; 5]; 95] = [
    [0b000, 0b000, 0b000, 0b000, 0b000], //
    [0b010, 0b010, 0b010, 0b000, 0b010], // !
    [0b101, 0b101, 0b000, 0b000, 0b000], // quote
    [0b101, 0b111, 0b101, 0b111, 0b101], // #
    [0b110, 0b011, 0b110, 0b011, 0b010], // $
    [0b001, 0b100, 0b010, 0b001, 0b100], // %
    [0b011, 0b011, 0b111, 0b101, 0b110], // &
    [0b010, 0b010, 0b000, 0b000, 0b000], // apostrophe
    [0b100, 0b010, 0b010, 0b010, 0b100], // (
    [0b001, 0b010, 0b010, 0b010, 0b001], // )
    [0b101, 0b010, 0b101, 0b000, 0b000], // *
    [0b000, 0b010, 0b111, 0b010, 0b000], // +
    [0b000, 0b000, 0b000, 0b010, 0b001], // ,
    [0b000, 0b000, 0b111, 0b000, 0b000], // -
    [0b000, 0b000, 0b000, 0b000, 0b010], // .
    [0b100, 0b100, 0b010, 0b001, 0b001], // /
    [0b110, 0b101, 0b101, 0b101, 0b011], // 0
    [0b010, 0b011, 0b010, 0b010, 0b010], // 1
    [0b011, 0b100, 0b010, 0b001, 0b111], // 2
    [0b011, 0b100, 0b010, 0b100, 0b011], // 3
    [0b101, 0b101, 0b111, 0b100, 0b100], // 4
    [0b111, 0b001, 0b011, 0b100, 0b011], // 5
    [0b110, 0b001, 0b111, 0b101, 0b111], // 6
    [0b111, 0b100, 0b010, 0b001, 0b001], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b100, 0b011], // 9
    [0b000, 0b010, 0b000, 0b010, 0b000], // :
    [0b000, 0b010, 0b000, 0b010, 0b001], // ;
    [0b100, 0b010, 0b001, 0b010, 0b100], // <
    [0b000, 0b111, 0b000, 0b111, 0b000], // =
    [0b001, 0b010, 0b100, 0b010, 0b001], // >
    [0b111, 0b100, 0b010, 0b000, 0b010], // ?
    [0b010, 0b101, 0b111, 0b001, 0b110], // @
    [0b010, 0b101, 0b111, 0b101, 0b101], // A
    [0b011, 0b101, 0b011, 0b101, 0b011], // B
    [0b110, 0b001, 0b001, 0b001, 0b110], // C
    [0b011, 0b101, 0b101, 0b101, 0b011], // D
    [0b111, 0b001, 0b111, 0b001, 0b111], // E
    [0b111, 0b001, 0b111, 0b001, 0b001], // F
    [0b110, 0b001, 0b111, 0b101, 0b110], // G
    [0b101, 0b101, 0b111, 0b101, 0b101], // H
    [0b111, 0b010, 0b010, 0b010, 0b111], // I
    [0b100, 0b100, 0b100, 0b101, 0b010], // J
    [0b101, 0b101, 0b011, 0b101, 0b101], // K
    [0b001, 0b001, 0b001, 0b001, 0b111], // L
    [0b101, 0b111, 0b111, 0b101, 0b101], // M
    [0b101, 0b111, 0b111, 0b111, 0b101], // N
    [0b010, 0b101, 0b101, 0b101, 0b010], // O
    [0b011, 0b101, 0b011, 0b001, 0b001], // P
    [0b010, 0b101, 0b101, 0b111, 0b110], // Q
    [0b011, 0b101, 0b111, 0b011, 0b101], // R
    [0b110, 0b001, 0b010, 0b100, 0b011], // S
    [0b111, 0b010, 0b010, 0b010, 0b010], // T
    [0b101, 0b101, 0b101, 0b101, 0b110], // U
    [0b101, 0b101, 0b101, 0b010, 0b010], // V
    [0b101, 0b101, 0b111, 0b111, 0b101], // W
    [0b101, 0b101, 0b010, 0b101, 0b101], // X
    [0b101, 0b101, 0b010, 0b010, 0b010], // Y
    [0b111, 0b100, 0b010, 0b001, 0b111], // Z
    [0b111, 0b001, 0b001, 0b001, 0b111], // [
    [0b000, 0b001, 0b010, 0b100, 0b000], // backslash
    [0b111, 0b100, 0b100, 0b100, 0b111], // ]
    [0b010, 0b101, 0b000, 0b000, 0b000], // ^
    [0b000, 0b000, 0b000, 0b000, 0b111], // _
    [0b001, 0b010, 0b000, 0b000, 0b000], // `
    [0b000, 0b011, 0b110, 0b101, 0b111], // a
    [0b001, 0b011, 0b101, 0b101, 0b011], // b
    [0b000, 0b110, 0b001, 0b001, 0b110], // c
    [0b100, 0b110, 0b101, 0b101, 0b110], // d
    [0b000, 0b110, 0b101, 0b011, 0b110], // e
    [0b100, 0b010, 0b111, 0b010, 0b010], // f
    [0b110, 0b101, 0b111, 0b100, 0b010], // g
    [0b001, 0b011, 0b101, 0b101, 0b101], // h
    [0b010, 0b000, 0b010, 0b010, 0b010], // i
    [0b000, 0b100, 0b100, 0b101, 0b010], // j
    [0b001, 0b101, 0b011, 0b011, 0b101], // k
    [0b011, 0b010, 0b010, 0b010, 0b111], // l
    [0b000, 0b111, 0b111, 0b111, 0b101], // m
    [0b000, 0b011, 0b101, 0b101, 0b101], // n
    [0b000, 0b010, 0b101, 0b101, 0b010], // o
    [0b011, 0b101, 0b101, 0b011, 0b001], // p
    [0b110, 0b101, 0b101, 0b110, 0b100], // q
    [0b000, 0b110, 0b001, 0b001, 0b001], // r
    [0b000, 0b110, 0b011, 0b110, 0b011], // s
    [0b010, 0b111, 0b010, 0b010, 0b110], // t
    [0b000, 0b101, 0b101, 0b101, 0b110], // u
    [0b000, 0b101, 0b101, 0b111, 0b010], // v
    [0b000, 0b101, 0b111, 0b111, 0b111], // w
    [0b000, 0b101, 0b010, 0b010, 0b101], // x
    [0b101, 0b101, 0b110, 0b100, 0b010], // y
    [0b000, 0b111, 0b110, 0b011, 0b111], // z
    [0b110, 0b010, 0b001, 0b010, 0b110], // {
    [0b010, 0b010, 0b000, 0b010, 0b010], // |
    [0b011, 0b010, 0b100, 0b010, 0b011], // }
    [0b110, 0b011, 0b000, 0b000, 0b000], // ~
];

const FIRST_PRINTABLE: u32 = 0x20;

fn compact_glyph(character: char) -> [u8; 5] {
    let folded = fold(character);
    let index = u32::from(folded).checked_sub(FIRST_PRINTABLE);
    match index.and_then(|index| COMPACT_FONT.get(index as usize)) {
        Some(bits) => *bits,
        // Unmapped characters read as a question mark, exactly like the 5x7
        // face. is_renderable is what keeps them out of the shipped deck.
        None => COMPACT_FONT[(u32::from('?') - FIRST_PRINTABLE) as usize],
    }
}

fn glyph(character: char) -> [u8; 5] {
    match fold(character) {
        'A' => [0x7e, 0x11, 0x11, 0x11, 0x7e],
        'B' => [0x7f, 0x49, 0x49, 0x49, 0x36],
        'C' => [0x3e, 0x41, 0x41, 0x41, 0x22],
        'D' => [0x7f, 0x41, 0x41, 0x22, 0x1c],
        'E' => [0x7f, 0x49, 0x49, 0x49, 0x41],
        'F' => [0x7f, 0x09, 0x09, 0x09, 0x01],
        'G' => [0x3e, 0x41, 0x49, 0x49, 0x7a],
        'H' => [0x7f, 0x08, 0x08, 0x08, 0x7f],
        'I' => [0x00, 0x41, 0x7f, 0x41, 0x00],
        'J' => [0x20, 0x40, 0x41, 0x3f, 0x01],
        'K' => [0x7f, 0x08, 0x14, 0x22, 0x41],
        'L' => [0x7f, 0x40, 0x40, 0x40, 0x40],
        'M' => [0x7f, 0x02, 0x0c, 0x02, 0x7f],
        'N' => [0x7f, 0x04, 0x08, 0x10, 0x7f],
        'O' => [0x3e, 0x41, 0x41, 0x41, 0x3e],
        'P' => [0x7f, 0x09, 0x09, 0x09, 0x06],
        'Q' => [0x3e, 0x41, 0x51, 0x21, 0x5e],
        'R' => [0x7f, 0x09, 0x19, 0x29, 0x46],
        'S' => [0x46, 0x49, 0x49, 0x49, 0x31],
        'T' => [0x01, 0x01, 0x7f, 0x01, 0x01],
        'U' => [0x3f, 0x40, 0x40, 0x40, 0x3f],
        'V' => [0x1f, 0x20, 0x40, 0x20, 0x1f],
        'W' => [0x3f, 0x40, 0x38, 0x40, 0x3f],
        'X' => [0x63, 0x14, 0x08, 0x14, 0x63],
        'Y' => [0x07, 0x08, 0x70, 0x08, 0x07],
        'Z' => [0x61, 0x51, 0x49, 0x45, 0x43],
        '0' => [0x3e, 0x51, 0x49, 0x45, 0x3e],
        '1' => [0x00, 0x42, 0x7f, 0x40, 0x00],
        '2' => [0x42, 0x61, 0x51, 0x49, 0x46],
        '3' => [0x21, 0x41, 0x45, 0x4b, 0x31],
        '4' => [0x18, 0x14, 0x12, 0x7f, 0x10],
        '5' => [0x27, 0x45, 0x45, 0x45, 0x39],
        '6' => [0x3c, 0x4a, 0x49, 0x49, 0x30],
        '7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        '8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        '9' => [0x06, 0x49, 0x49, 0x29, 0x1e],
        '-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        '_' => [0x40, 0x40, 0x40, 0x40, 0x40],
        '.' => [0x00, 0x60, 0x60, 0x00, 0x00],
        ':' => [0x00, 0x36, 0x36, 0x00, 0x00],
        '/' => [0x20, 0x10, 0x08, 0x04, 0x02],
        '+' => [0x08, 0x08, 0x3e, 0x08, 0x08],
        ',' => [0x00, 0x40, 0x30, 0x00, 0x00],
        '\'' => [0x00, 0x00, 0x03, 0x00, 0x00],
        '"' => [0x00, 0x03, 0x00, 0x03, 0x00],
        '(' => [0x00, 0x1c, 0x22, 0x41, 0x00],
        ')' => [0x00, 0x41, 0x22, 0x1c, 0x00],
        '&' => [0x36, 0x49, 0x55, 0x22, 0x50],
        '!' => [0x00, 0x00, 0x5f, 0x00, 0x00],
        ';' => [0x00, 0x56, 0x36, 0x00, 0x00],
        '=' => [0x14, 0x14, 0x14, 0x14, 0x14],
        '<' => [0x08, 0x14, 0x22, 0x41, 0x00],
        '>' => [0x00, 0x41, 0x22, 0x14, 0x08],
        '%' => [0x23, 0x13, 0x08, 0x64, 0x62],
        '#' => [0x14, 0x7f, 0x14, 0x7f, 0x14],
        '*' => [0x14, 0x08, 0x3e, 0x08, 0x14],
        '?' => [0x02, 0x01, 0x51, 0x09, 0x06],
        '@' => [0x3e, 0x41, 0x5d, 0x55, 0x1e],
        '|' => [0x00, 0x00, 0x7f, 0x00, 0x00],
        '$' => [0x24, 0x2a, 0x7f, 0x2a, 0x12],
        '[' => [0x00, 0x7f, 0x41, 0x41, 0x00],
        ']' => [0x00, 0x41, 0x41, 0x7f, 0x00],
        '{' => [0x00, 0x08, 0x36, 0x41, 0x00],
        '}' => [0x00, 0x41, 0x36, 0x08, 0x00],
        '^' => [0x04, 0x02, 0x01, 0x02, 0x04],
        '~' => [0x08, 0x04, 0x08, 0x10, 0x08],
        '\\' => [0x02, 0x04, 0x08, 0x10, 0x20],
        ' ' => [0; 5],
        // Anything unmapped reads as a question mark rather than a blank or a
        // box. `is_renderable` is what keeps unmapped characters out of the
        // shipped deck.
        _ => [0x02, 0x01, 0x51, 0x09, 0x06],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(canvas: &Canvas) -> usize {
        (0..WIDTH)
            .flat_map(|x| (0..HEIGHT).map(move |y| (x, y)))
            .filter(|&(x, y)| canvas.is_lit(x, y))
            .count()
    }

    #[test]
    fn instruction_block_matches_the_hand_tuned_offsets() {
        // The table this replaced read {1 => 44, 2 => 40, 3 => 36, _ => 33}.
        assert_eq!(instruction_y(2, 0), 40);
        assert_eq!(instruction_y(3, 0), 36);
        assert_eq!(instruction_y(4, 0), 33);
    }

    #[test]
    fn instruction_block_never_runs_off_the_panel() {
        for count in 1..=MAX_INSTRUCTIONS {
            let last = instruction_y(count, count - 1);
            assert!(
                last + COMPACT_HEIGHT <= HEIGHT,
                "{count} instructions overflow: last row {last}"
            );
            assert!(
                instruction_y(count, 0) >= INSTRUCTION_TOP,
                "{count} instructions collide with the detail line"
            );
        }
    }

    /// Every character the badge can draw must be distinguishable at 3x5. The
    /// mechanical downsample this replaced rendered H as N and 0 as 8.
    #[test]
    fn the_compact_face_never_draws_two_characters_alike() {
        let used = " ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.:/+,'\"()&!?;=<>%#*@|$[]{}^~\\";
        for left in used.chars().filter(|c| *c != ' ') {
            assert_ne!(
                compact_glyph(left),
                [0; 5],
                "{left:?} draws nothing in the compact face"
            );
            for right in used.chars().filter(|c| *c != ' ') {
                if left != right {
                    assert_ne!(
                        compact_glyph(left),
                        compact_glyph(right),
                        "compact {left:?} and {right:?} are the same pixels"
                    );
                }
            }
        }
    }

    #[test]
    fn punctuation_that_the_deck_contains_is_drawn_and_distinguishable() {
        let punctuation = ",'\"()&!?;@|$[]{}^~";
        for character in punctuation.chars() {
            assert!(is_renderable(character), "{character:?} is not renderable");
            assert_ne!(
                glyph(character),
                [0; 5],
                "{character:?} draws nothing at all"
            );
        }
        // A comma that renders like an apostrophe is no better than one that
        // renders like a question mark.
        for left in punctuation.chars() {
            for right in punctuation.chars() {
                if left != right {
                    assert_ne!(
                        glyph(left),
                        glyph(right),
                        "{left:?} and {right:?} draw the same glyph"
                    );
                }
            }
        }
    }

    #[test]
    fn accents_fold_onto_ascii() {
        assert_eq!(fold('É'), 'E');
        assert_eq!(fold('ü'), 'U');
        assert_eq!(fold('ñ'), 'N');
        assert_eq!(fold('\u{2019}'), '\'');
        assert_eq!(fold('\u{201C}'), '"');
        assert_eq!(fold('\u{2014}'), '-');
        for character in "ÉüñÀøß".chars() {
            assert!(is_renderable(character));
        }
    }

    #[test]
    fn a_tie_names_the_first_winner_and_counts_the_rest() {
        let winners: Vec<String> = ["KEEN-RAVEN-C8", "TIDY-FALCON-A2", "SPRY-LEMUR-77"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let line = winner_line(&winners);
        assert_eq!(line, "WINNER KEEN-RAVEN-C8 +2 TIED");
        assert!(
            line.chars().count() <= MAX_COMPACT_CHARS,
            "winner line still overflows: {} chars",
            line.chars().count()
        );
        assert_eq!(winner_line(&[]), "NO WINNER");
        assert_eq!(winner_line(&["SIM-05".to_owned()]), "WINNER SIM-05");
    }

    #[test]
    fn every_winner_line_fits_ten_long_callsigns() {
        let winners: Vec<String> = (0..10)
            .map(|index| format!("KEEN-RAVEN-{index:02}"))
            .collect();
        assert!(winner_line(&winners).chars().count() <= MAX_COMPACT_CHARS);
    }

    #[test]
    fn feedback_reports_the_value_temporal_will_record() {
        let mut doubled = Canvas::new();
        doubled.feedback("SIM-05", true, 2);
        let mut single = Canvas::new();
        single.feedback("SIM-05", true, 1);
        assert_ne!(
            lit(&doubled),
            lit(&single),
            "the badge draws the same pixels for +1 and +2"
        );
    }

    /// The deck contains unbroken tokens longer than a line: `Vec<T>` answers
    /// and `<marquee></marquee>`. These used to lose their tail entirely.
    #[test]
    fn wrap_breaks_long_tokens_instead_of_dropping_them() {
        let lines = wrap("<marquee></marquee>", 11, 2);
        assert_eq!(lines, vec!["<marquee></", "marquee>"]);
        assert_eq!(lines.concat(), "<marquee></marquee>");

        // A token longer than every available line still fills what it can.
        let clipped = wrap("aaaaaaaaaaaaaaaaaaaaaaaaaaaa", 11, 2);
        assert_eq!(clipped.len(), 2);
        for line in &clipped {
            assert!(line.chars().count() <= 11);
        }
    }

    #[test]
    fn wrap_keeps_words_whole_and_respects_the_line_budget() {
        let lines = wrap("THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG", 11, 3);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.chars().count() <= 11, "{line:?} is too wide");
        }
    }

    #[test]
    fn every_status_screen_draws_and_fits_the_panel() {
        for status in Status::ALL {
            let mut canvas = Canvas::new();
            canvas.status("KEEN-RAVEN-C8", status);
            assert!(lit(&canvas) > 0, "{:?} drew an empty panel", status);
            assert!(
                status.instruction().chars().count() <= MAX_COMPACT_CHARS,
                "{:?} instruction is {} chars, panel fits {MAX_COMPACT_CHARS}",
                status,
                status.instruction().chars().count()
            );
        }
    }

    #[test]
    fn status_headlines_are_distinct() {
        // ALL is what preview renders; a duplicated headline would silently
        // drop a screen from the contact sheet.
        let mut seen = std::collections::BTreeSet::new();
        for status in Status::ALL {
            assert!(
                seen.insert(status.headline()),
                "duplicate headline {:?}",
                status.headline()
            );
        }
        assert_eq!(seen.len(), Status::ALL.len());
    }

    #[test]
    fn screens_draw_something_and_stay_inside_the_panel() {
        let question = Question {
            id: "q-1".to_owned(),
            category: "rust".to_owned(),
            difficulty: "easy".to_owned(),
            prompt: "WHEN WAS \"LUIGI'S MANSION 3\" RELEASED?".to_owned(),
            answers: [
                "2019".to_owned(),
                "2001".to_owned(),
                "2013".to_owned(),
                "2021".to_owned(),
            ],
            correct_index: 0,
        };
        let mut canvas = Canvas::new();
        canvas.question("SIM-05", &question);
        assert!(lit(&canvas) > 0);

        for screen in [
            Canvas::waiting as fn(&mut Canvas, &str),
            Canvas::sleeping,
            Canvas::panic,
            Canvas::recovered,
        ] {
            let mut canvas = Canvas::new();
            screen(&mut canvas, "KEEN-RAVEN-C8");
            assert!(lit(&canvas) > 0);
        }
    }
}

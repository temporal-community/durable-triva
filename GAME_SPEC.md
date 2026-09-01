# Game contract

- The operator starts one 60-second game from the Mac web UI mirrored to a TV.
- Physical badges and phone players compete in the same Workflow, score table,
  and question pool. Any participant may join late at score zero; there is no
  frozen roster. Badge-only and phone-only rounds are both valid.
- The TV attract loop carries a QR code for the stable Cloud Run phone URL. A
  phone receives a deterministic badge-style callsign in a long-lived cookie;
  there is no login or shared identity database.
- Correct answers score `+1`. Wrong answers score `-1` and complete the
  Activity normally; the question is consumed. During double-points chaos
  those values become `+2` and `-2`. Activity retries are reserved for genuine
  Worker loss and heartbeat timeout.
- Holding LEFT+RIGHT for 500 ms simulates a Worker crash by suppressing
  heartbeats for 16 seconds. Temporal's 15-second heartbeat timeout retries the
  Activity while the original badge is still unavailable. The badge
  refuses that question for the rest of the game, allowing another Worker to
  recover it. Panic itself scores `0`.
- The controller counts active ESP32 Activity pollers when a round starts. The
  Workflow keeps one outstanding Activity per registered participant, with a
  minimum of one bootstrap Activity for a phone-only round. Phone joins grow
  that target during the round. Retrying unfinished work is not a duplicate;
  it may wait briefly for a Worker rather than leaving a healthy participant
  idle throughout normal play. The API may override the target for diagnostics.
- Phones do not impersonate SDK Workers. A Rust Temporal Worker on a GCP Cloud
  Run Worker Pool receives real Activities, assigns them durably through the
  Game Workflow, and returns `WillCompleteAsync`. The stateless phone API
  heartbeats and completes those Activities by Workflow, run, and Activity ID.
  Every phone session owns at most one outstanding Activity.
- Holding the phone's SIMULATE CRASH control for 500 ms stops heartbeats for
  six seconds. Temporal's five-second heartbeat timeout retries the unfinished
  Activity. The crashing phone cannot reclaim that question during the round,
  and its recovery screen lasts for the same interval as the badge behavior.
- The global deadline cancels outstanding Activities for zero points. There is
  no per-badge timer. Ties create shared winners.
- Callsigns derive deterministically from the factory MAC and survive reboots.
  NVS retains the active game and abandoned-question IDs.
- From the idle Worker screen, holding DOWN for three seconds enters ESP32 deep
  sleep. Releasing before the countdown completes cancels it. Any face button
  wakes the badge through its direct RTC GPIO, with the hardware's shared
  revision wake lines retained as fallbacks. Sleep is not armed while an
  Activity owns the answer controls.
- The OLED uses wrapped questions, a compact 2x2 answer grid, and positional
  Nintendo-style glyphs rather than bare button letters.
- A badge draws an assigned question before sending best-effort assignment
  telemetry. Game Signals have a 750 ms UI-path ceiling, so a slow Cloud round
  trip cannot leave the screen looking frozen. Between Activities the OLED
  explicitly says it is polling Temporal and that the next question is
  automatic.
- The TV is a fixed 16:9 race board: a header band carrying the round timer and
  live counters, the badge lanes, and a detail rail. Each lane contains
  callsign, rank, Worker state, score, and a score bar drawn as a routed trace
  normalised to the spread of the field, falling back to leader-relative while
  that spread is too small to stretch honestly. Lanes are ordered by score, so
  reading order is always first place to last; they resettle at most once per
  800 ms and the swap animates, and rank is derived from the same settle so a
  lane's number can never disagree with its position. The final board freezes
  in place and labels all tied winners. The rail carries the last resolved
  answer and a rolling feed of durable events, and switches to a round summary
  when the round closes. Above six badges the lanes split into two columns and
  the rail becomes a bottom band.
- Routine answers never enter the durable-events feed. Ten badges answer several
  times a second, which would evict every fault, handoff and powerup from the
  window, so the snapshot tags each event with a kind and routine answers are
  the first dropped when the window is full. Throughput is reported as an
  activity rate in the header instead. A lane speaks only for a wrong answer, a
  crash or a retry pickup, and otherwise shows resting telemetry.
- Selecting New Round first opens a three-part booth attract loop explaining
  Rust Workers, Temporal Activity retry, and the 60-second rules. Starting the
  next Workflow remains a separate deliberate operator action. The finished
  board remains visible for 30 seconds, then enters this attract loop
  automatically; selecting New Round enters it immediately.
- The attract loop defaults to the original astronaut tardigrade and explains
  the physical badge Worker path. Phone joining is opt-in: an operator button
  swaps the artwork for the large QR code and can switch straight back. The TV
  does not carry an always-visible QR during badge-first booth operation.
- Operator controls execute validated Workflow Updates for ten seconds of
  double points, ten seconds of Rust-only scheduling, sudden death on the next
  correct answer, and one `+30 seconds` timer extension. The three gameplay
  modifiers are mutually exclusive; the timer extension is independent. Each
  Update also records a monotonically numbered power-up notice in Workflow
  state. Awake badges query that durable state, vibrate, display it for 1.5
  seconds, suppress answer input during the overlay, and restore their prior
  question or waiting screen.
- The supervised Mac controller may be deliberately crashed. The browser keeps
  the frozen board visible while `run-web.sh` restarts the Rust process and the
  Workflow Worker rebuilds game state from Temporal history. Recovery steps
  advance only after observing process loss, a new process identity, a
  successful Temporal query, and a restored snapshot that continues the frozen
  board: same game, every badge still present, and no badge's counters going
  backwards. The badges keep answering while the Mac Worker restarts, so the
  restored state is expected to have moved on rather than match digest for
  digest; both digests are shown as evidence. Recovery is also bounded by a
  timeout so the board can never be left frozen.
- Finished round summaries are written to Temporal Memo and listed through
  Visibility; no game-history database is required. Typed Search Attributes
  are optional when the Cloud API key has namespace-operator permission.
- The deck is 30% Rust, 15% Temporal, 15% mental math, and 40% family-friendly
  general trivia. Temporal questions are mostly introductory. The Workflow
  exhausts a shuffled deck before refilling it, so a retry preserves its
  question and ordinary duplicates do not enter the pool early.
- The phone path is a stateless Rust Cloud Run service plus a Rust Temporal
  Serverless Worker Pool. The Mac continues to run the TV, operator API, and
  Workflow Worker. The Worker Pool may scale to zero; the operator runs a demo
  before stage use to warm it. `PUBLIC_BASE_URL` supplies the QR target.
- The phone API batches assignment discovery into one disposable roster cache
  refresh every 250 ms. Browser state polls never fan out into individual
  Temporal queries. Heartbeats and answers address asynchronous Activities
  directly by Workflow ID, Run ID, and Activity ID. An API restart rebuilds
  the cache from a Workflow query; durable state never lives only in memory.
- Worker Versioning is a deliberate follow-up, not part of the basic release.
  The eventual demo will be triggered from a mirrored terminal command and
  will preserve gameplay mechanics across versions.
- Acceptance target: 100 simultaneous phones, ten simulated badge Workers,
  and two physical badges across three consecutive rounds. Assignment latency
  after warm-up must remain below two seconds, and taking the entire phone/GCP
  path offline must not stop badge-only play.
- Demo-ready acceptance requires two physical badges proving that a fake crash
  produces a heartbeat timeout and moves the same question to the other badge
  as a higher real Temporal Activity attempt.

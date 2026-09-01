# Engineering journal

## 2026-08-30 — Phone-player architecture locked

- Chose one mixed game rather than a separate crowd mode: physical badges and
  phones share the same 60-second Workflow, score table, question deck, chaos
  controls, and retry semantics. Late phone joins start at zero.
- Rejected treating browsers as Temporal Workers. A Rust Activity Worker on a
  GCP Cloud Run Worker Pool will dispatch real Activities to phones using
  asynchronous Activity completion. The phone API and UI are a separate
  stateless Cloud Run service; the Mac remains the Workflow Worker, operator
  server, and TV scoreboard.
- Rejected DynamoDB and Firestore for the first release. A long-lived cookie
  holds only an anonymous badge-style identity. The Game Workflow owns phone
  registration, assignment, scores, abandoned questions, and retry state.
- Locked one outstanding Activity per phone session. The phone heartbeats its
  async Activity through the API. Holding SIMULATE CRASH for 500 ms suppresses
  heartbeats for six seconds so the existing five-second heartbeat timeout
  produces a real Temporal retry; the phone cannot reclaim that question.
- The TV will show a Cloud Run URL as a QR code. The public service embeds the
  portrait phone UI and exposes the phone API from one binary. No TV hosting,
  identity service, or external database is added.
- The initial load target is 100 phones, ten simulated badge Workers, and two
  physical badges for three consecutive rounds. Phone simulation uses seeded
  1–10 second answer delays, 80% correct answers, 20% wrong answers, and 5%
  real crash timing.
- GCP Worker Versioning remains a planned stage beat, but was explicitly
  deferred until the basic phone path works. The eventual v2 deployment will
  be triggered from a mirrored terminal command, with a visible recovery UI
  change and identical scoring/timing.

## 2026-08-30 — First 100-phone Temporal Cloud validation

- Implemented the phone UI/API, Rust Activity dispatcher, async Activity
  completion, cookie callsigns, TV QR, 500 ms crash hold, six-second blackout,
  deterministic deck recycling, and a seeded 100-session simulator with 1–10
  second answer delays and intentionally wrong answers.
- The first load attempts exposed two authentic scaling failures. Repeatedly
  signaling `phone_joined` on every browser poll bloated Workflow traffic, and
  querying assignment ownership from every heartbeat plus every dispatcher
  Activity filled Temporal's consistent-query buffer. The exact Cloud error
  was `Some resource has been exhausted: consistent query buffer is full`.
- Removed per-phone and per-Activity query fan-out. The API now refreshes one
  disposable roster cache from a batched Workflow query every 250 ms, while
  heartbeats and answers use the async Activity identifier directly. The
  dispatcher heartbeats once, signals durable readiness, and immediately
  returns `WillCompleteAsync` rather than polling a query until assignment.
- Final live round `trivia-4b4c8989a2544876921410795767d40a` completed 100
  simulated sessions: 377 accepted answers and 26 deliberate crashes. The
  Workflow recorded 327 completed questions, 264 correct, 63 wrong, 28
  heartbeat timeouts, 28 reassignments, and 455 Activity attempts. One manually
  open phone was also registered but idle, accounting for the two timeouts
  beyond the simulator's 26 crashes.
- All 17 host tests pass after the changes. Cloud Run deployment and the
  terminal-triggered Worker Versioning beat remain unvalidated follow-up work;
  this session proved the local Rust processes against Temporal Cloud, not a
  deployed GCP service.
- Added one production container containing both phone binaries plus documented
  Cloud Run service and Worker Pool commands. Local image validation could not
  start because Docker Desktop was not running: `failed to connect to the
  docker API at unix:///Users/shy/.docker/run/docker.sock`. The Dockerfile and
  GCP deployment are therefore prepared but not yet build-validated.
- Corrected the phone client's font registration after a live screenshot showed
  mismatched heavy question text. The Space Grotesk asset is a variable font,
  but its `@font-face` had been registered only as normal weight, causing the
  browser to synthesize the requested 600 and 700 weights. Declared its real
  300–700 range, declared Space Mono at 400, replaced leftover unregistered
  family aliases, and disabled synthetic font styles. A real live question at
  390×844 loaded both bundled faces at the intended weights with no overflow.

## 2026-08-31 — Badge-first attract screen restored

- Restored the original astronaut tardigrade artwork and the original
  badge-focused three-part attract story. Removed the always-visible header QR
  so the default booth state speaks clearly about physical Rust Workers.
- Added a secondary `Show phone QR` control beside Start Round. It swaps the
  artwork in place for the large QR and becomes `Show badge art`; entering a
  new intermission always resets to badge art.
- Browser-validated both views in the live controller. Artwork/QR visibility,
  button state, and ARIA pressed state all switch together, neither view
  overflows, and the controller was left in badge mode.

## 2026-08-31 — Root README reorganized around the demo story

- Reformatted the root README to follow Durable Wordle's documentation shape:
  a direct demo pitch, a Temporal concepts table, a Mermaid architecture map,
  fast runnable paths, deployment guidance, development gates, and a concise
  tech-stack summary.
- Kept Durable Trivia's badge, phone, simulator, Cloud Run, and operator details
  rather than copying Durable Wordle's Python-specific instructions. The deeper
  firmware and web guides remain the source for component setup.

## 2026-08-31 — Root README reduced to an entry point

- Follow-up review found that the first README pass still duplicated the
  component guides and was too dense for a new reader. Reduced it to the demo
  pitch, Temporal concepts, architecture, a three-step simulated-badge start,
  project layout, and a documentation map.
- Detailed firmware, controller, phone, Cloud Run, operator, verification, and
  game-rule material now lives behind direct links to `firmware/README.md`,
  `web/README.md`, and `GAME_SPEC.md` instead of being repeated at the root.

## 2026-08-31 — Official Temporal logo added as a shared web asset

- Added Temporal's official horizontal light SVG lockup from the supplied
  Contentful asset URL. The downloaded source is path-only SVG with no scripts,
  embedded images, or external links.
- Embedded and exposed the logo from both Rust web binaries at
  `/assets/temporal-logo-horizontal-light.svg`, keeping deployment behavior
  consistent with the existing fonts and astronaut artwork.
- Placed the lockup in the attract-screen header's upper-right corner opposite
  the Durable Trivia wordmark. It replaces the otherwise-hidden idle meters,
  then yields that grid position back to live question and handoff telemetry
  when a round begins.

## 2026-08-17 — First playable Temporal trivia build

- Created a standalone Git repository with two product folders: `firmware/`
  for the ESP32-S3 Rust Worker and `web/` for the Rust Workflow Worker,
  controller API, SSE feed, TV UI, and committed question data.
- Kept the Activity Worker boundary already proven on this badge. The firmware
  uses the locally patched Temporal Rust SDK 0.5.0 needed for ESP-IDF, connects
  to Temporal Cloud with API-key authentication and verified server TLS, and
  derives a stable callsign from the factory MAC.
- Implemented the OLED question UI with word wrapping and the original badge's
  positional Nintendo-style glyphs. Implemented TOP, RIGHT, LEFT, and DOWN
  answer input plus a 500 ms LEFT+RIGHT simulated crash, three-second recovery,
  retryable Activity failure, and NVS-backed question refusal.
- Limited each physical Worker to one concurrent Activity. The SDK tuner
  otherwise defaults to enough Activity slots for multiple questions to race
  over the badge's single display and button cluster.
- Implemented the durable 60-second Workflow, dynamic
  `max(10, active_badges * 2)` backlog, scoring, tied winners, latest-answer
  spotlight, and single-game controller guard. The HTTP start path reserves the
  game before its Cloud call so simultaneous clicks cannot start two games.
- Committed an Open Trivia DB snapshot from
  `leakyhose/open-trivia-script-data`, attributed under CC BY-SA 4.0, and mixed
  it with authored Rust, Temporal, and generated math questions. Tests enforce
  the first 100 as 30 Rust, 15 Temporal, 15 math, and 40 general questions,
  reject display overflow, and reject duplicates.
- Host tests passed: 5 tests, 0 failures. The Rust web controller connected to
  the configured Temporal Cloud namespace and served both `/api/state` and the
  TV page at `127.0.0.1:3000`. Browser inspection found no page overflow at a
  1280x720 viewport.
- The first live controller connection failed with `Connecting to HTTPS without
  TLS enabled`. The endpoint was HTTPS, but the Mac client omitted
  `TlsOptions`; adding default server TLS fixed the connection. API-key auth
  does not remove Temporal Cloud's TLS requirement.
- The first real smoke Workflow then failed every Workflow task with
  `[TMPRL1100] Nondeterministic future detected` because the implementation
  uses `FuturesUnordered` to maintain the dynamic Activity backlog. SDK Core's
  own `wait_condition_waker_in_futures_unordered` test documents that its
  forwarding wakers fall outside the detector guard and disables the detector
  for that case. Applied the same narrow Worker-level opt-out; every future in
  this Workflow remains an SDK Activity or SDK timer.
- The firmware release build passed in 2m05s. An initial `espflash save-image`
  check incorrectly reported the 8 MB image against a 4 MB app partition
  because the command omitted the custom table. Passing `firmware/partitions.csv`
  confirmed 8,000,128 / 14,680,064 bytes (54.50%). `flash-badge.sh` now always
  passes the explicit 16 MB badge layout.
- Live flashing is still pending. macOS currently exposes only Bluetooth,
  debug-console, and earbud serial ports; no Espressif `/dev/cu.usbmodem*`
  device is present. No flash attempt was made against an unresolved target.
- After disabling the detector for this documented combinator case, the
  previously stuck smoke Workflow replayed and completed. A fresh fixed-ID
  Cloud round scheduled 10 unique Activities, populated the 60-second global
  deadline, rejected a concurrent start with HTTP 409, and completed with
  `Round finished with no answers` at the deadline.
- Replaced UUID Workflow IDs with stable ID `temporal-trivia-active`, explicit
  `AllowDuplicate` reuse after completion, and `Fail` conflict behavior while
  running. Restarting the Rust controller restored the finished snapshot from
  a Temporal query, proving the TV state no longer depends only on process
  memory. The controller was left running at `127.0.0.1:3000`.
- The badge later appeared at `/dev/cu.usbmodem101`. Flashed the validated image
  to ESP32-S3 revision 0.2; the bootloader confirmed 16 MB flash and the 14 MB
  factory partition. Live boot passed the 8 MB PSRAM test, rendered through the
  OLED driver without error, connected to Wi-Fi at `192.168.1.103`, synchronized
  time, completed verified Temporal Cloud TLS, and polled
  `temporal-trivia-badges-v1` as `esp32-e83dc1f94c70` / `KEEN-SEAL-70`.
- Ran a real hardware round. The Workflow recorded `KEEN-SEAL-70 joined`, six
  completed questions, two correct answers, four wrong answers, one simulated
  crash, a recovery event, continued work after recovery, a final score of
  `-2`, and `KEEN-SEAL-70` as winner. This validates real Activity dispatch,
  button answers, scoring, retry/recovery, deadline completion, and final
  winner publication. The serial monitor was detached without stopping the
  badge Worker; the Mac controller remains live at `127.0.0.1:3000`.
## 2026-08-17 — TV UI rebaseline

- Two visual passes based on generic PCB styling and the production backplate render were rejected because they treated the badge as decoration instead of matching its interface.
- Discarded both passes and rebaselined from `badge-ui-review-kathy-20260429-final.zip`, `GridMenuScreen.cpp`, `OLEDLayout.cpp`, and the firmware button-glyph generator.
- The new TV UI uses the badge's actual one-bit black/white language: thick rules, 2-column menu proportions, inverse-color selected cell, asymmetric rounded corners, compact status header/footer, and the exact 10x10 confirm-cluster bitmap geometry. It does not use the badge render as wallpaper.
- Inline JavaScript syntax and `cargo check --offline -p temporal-trivia-web --target aarch64-apple-darwin` passed. Browser inspection at the mirrored 1164x655 viewport showed no document overflow and preserved the durable finished-round state.

## 2026-08-17 — Badge deep sleep

- Added the original badge's idle-only hold-DOWN sleep gesture: the countdown
  arms after 250 ms and enters ESP32 deep sleep after 3 seconds. Releasing
  early returns to the Worker waiting screen.
- Sleep is gated by an Activity-active guard, so DOWN remains a trivia answer
  while a question owns the controls and cannot accidentally start shutdown.
- Wake uses the Echo hardware's diode-OR `INT_PWR_PIN` on RTC GPIO13, not the
  four button GPIOs directly. This lets any face button wake the badge while
  avoiding GPIO0/LEFT's boot-strapping role.
- The OLED blanks and receives display-off before sleep. The release gate
  prevents the held shutdown button from causing an immediate wake bounce.
- The release firmware built in 2m19s and fit the factory partition at
  8,015,360 / 14,680,064 bytes (54.60%). It was flashed to ESP32-S3 revision
  0.2 badge `e8:3d:c1:f9:4c:70`; live button sleep/wake confirmation is
  pending the physical press test.

## 2026-08-17 — Deep-sleep wake correction and recovery

- The first physical test entered deep sleep but did not wake. Comparing the
  implementation again with the original Echo `Power.cpp` found that the
  original arms both revision-dependent diode-OR wake lines, GPIO10 and
  GPIO13; the first implementation had copied only GPIO13.
- Corrected wake setup to configure both RTC GPIOs as pulled-up inputs and use
  an active-low EXT1 any-low mask. Individual face-button GPIOs remain
  intentionally excluded because LEFT is the GPIO0 boot strap.
- The corrected release image built at 8,015,504 / 14,680,064 bytes (54.60%).
  `espflash` then panicked while parsing serial data, and two esptool stub-mode
  attempts lost USB around 3%. A no-stub, uncompressed ROM-loader write
  completed all 8,015,504 bytes in 73.1 seconds and verified the flash hash.
- After that recovery write, the badge remained blank and silent while still
  enumerating as USB Serial/JTAG. An explicit ROM `run` reset also produced no
  application output. A clean USB power cycle is the next diagnostic gate;
  corrected sleep/wake behavior is not yet physically validated.

## 2026-08-17 — Direct-button wake verified

- The clean power cycle exposed `Invalid image block, can't boot.` The ROM
  loader had verified that the offline merged file was written accurately, but
  the merged file itself was invalid for this app layout. Reflashing the ELF
  through `espflash` with the explicit 16 MB partition table restored a normal
  boot; the no-stub recovery method should not reuse that merged artifact.
- A second physical test proved GPIO10/GPIO13 still did not respond to UP on
  this badge revision. Wake now arms the four actual active-low face-button
  GPIOs (0, 7, 17, 18) plus GPIO10/GPIO13 as revision fallbacks.
- The final build was 8,015,568 / 14,680,064 bytes (54.60%). On hardware, a
  3-second DOWN hold entered deep sleep, tapping UP caused an EXT1 wake and
  normal app boot, PSRAM passed, the OLED returned, Wi-Fi reacquired
  `192.168.1.103`, and the Worker resumed polling
  `temporal-trivia-badges-v1`.
- UP wake is physically verified. RIGHT and DOWN use the same direct-wake
  configuration. LEFT is also armed, but remains a separate validation case
  because its GPIO0 line is an ESP32 boot strap.

## 2026-08-17 — Durable failure, chaos, and round history

- Expanded the demo around four Temporal behaviors selected for the booth:
  heartbeat-timeout reassignment, a deliberately crashable Mac Worker,
  durable operator chaos controls, and history for completed rounds. Wrong
  answers now count as errors, apply the score penalty, and return the same
  question to Temporal instead of completing its Activity.
- Badge Activities keep the existing 5-second heartbeat timeout. Holding LEFT
  and RIGHT for 500 ms now abandons the local question and suppresses
  heartbeats for 6 seconds, allowing Temporal to time it out and dispatch the
  retry before the original badge resumes polling. Wrong answers use a
  retryable Activity failure and are also abandoned locally so that badge does
  not immediately reclaim the same question.
- Added durable Signals for 10 seconds of double points, 10 seconds of
  Rust-only questions, one 30-second extension, and sudden death. A live Cloud
  round accepted double points, Rust-only, and the extension; the deadline
  moved from 60 to 90 seconds and all three commands appeared in Workflow
  state. Sudden death is covered by the same implementation but was not
  exercised on hardware in this session.
- Added `run-web.sh` as a small supervisor. The operator crash endpoint exits
  with code 75, waits two seconds, and starts the Rust process again against
  the same Temporal state. Three live crash tests recovered successfully; the
  first cold rebuild took about 14 seconds and cached restarts took about four.
- Temporal Cloud rejected Search Attribute administration with `Request
  unauthorized.` The current API key can execute Workflows but cannot register
  namespace attributes. `configure-visibility.sh` preserves the optional
  registration path, while the default implementation stores a compact round
  summary in Workflow Memo and reads it through Visibility without elevated
  namespace permission.
- Two other history approaches were rejected. Cloud returned `Client specified
  an invalid argument` because this namespace does not support the attempted
  `ORDER BY` clause. Replaying old runs with the changed Workflow code exposed
  `[TMPRL1100] Nondeterminism error: Timer machine does not handle this event:
  HistoryEvent(id: 25, ActivityTaskScheduled)`. Memo avoids replaying old
  histories; pre-Memo runs are skipped.
- Rebuilt the mirrored-TV interface from scratch as a fixed 16:9, minimal PCB
  race board using local fonts and cropped real PCB layers. It retains stable
  first-seen lane positions, a single timer, restrained gold score flashes,
  frozen winner labels, a visible start test pad, and an operator drawer for
  chaos, history, and Mac Worker recovery. Browser checks passed at 1164x655,
  1920x1080, and a letterboxed 1400x700 viewport with no overflow.
- Host tests pass 7/7. The release firmware built at 8,040,176 / 14,680,064
  bytes (54.77%), flashed successfully, booted, joined Wi-Fi, and resumed
  polling Temporal Cloud as `KEEN-SEAL-70`.
- Remaining physical gate: no wrong-answer button press happened during the
  validation rounds, and only one badge was connected. The retry mechanics are
  implemented and host-tested, but a visible timeout handoff from one physical
  badge to another still needs a second badge.

## 2026-08-17 — Public setup and release preparation

- Reworked the README into a clean-room deployment path instead of relying on
  the private sibling `TrafficLight/.env`. Added ignored, repository-local
  Temporal and Wi-Fi examples, environment-variable overrides, ESP Rust
  toolchain installation, explicit firmware output and serial-port discovery,
  the required 16 MiB factory-partition flash command, controller startup,
  operator controls, badge controls, and verification commands.
- Made the build and flash scripts portable across normal `espup`, ESP-IDF, and
  Cargo tool locations. The scripts retain local tool fallbacks for this
  checkout, accept explicit overrides, and fail with an actionable install
  command when a tool is missing. The web supervisor now derives the host
  target instead of hard-coding Apple Silicon.
- Configuration fails before compiling firmware when a required Wi-Fi or
  Temporal value is blank. The current workspace keeps its legacy credential
  fallback, while a fresh clone uses `.env.temporal` or exported variables.
- Release checks passed: four shell scripts parse with `sh -n`, host tests pass
  7/7, formatting and `git diff --check` pass, and the release firmware rebuilt
  successfully in 2m18s with the existing seven dead-code warnings.
- GitHub publication is currently blocked outside the source tree: this local
  repository has no `origin`, and `gh auth status` reports that the active
  `Shy` token is invalid. No files were staged or pushed before resolving that
  ownership/authentication boundary.

## 2026-08-17 — GitHub authentication repaired

- The apparent recurring token expiration was an incomplete OAuth device flow,
  not an expiring stored token. `hosts.yml` retained the active `Shy` account
  name, but `gh auth token` could not retrieve a credential and the macOS
  Keychain had no GitHub CLI entry. The prior one-time device code then expired
  while the CLI waited for approval.
- Removed only the stale local `gh` account entry and completed a fresh browser
  login. Final verification reports `Shy (keyring)`, API user `Shy`, retrievable
  credentials, and the expected `repo`, `read:org`, and `gist` scopes.
- Switched Git operations to SSH. A direct GitHub SSH probe authenticated as
  `Shy`, so pushes no longer depend on HTTPS credential handling while GitHub
  API operations continue using the OAuth token stored in Keychain.

## 2026-08-17 — Public repository secret audit

- Before and after publishing, scanned the current tree and complete local Git
  history for Temporal API keys, Wi-Fi passwords, GitHub tokens, AWS-style
  keys, and private-key headers. No real credential material or private-key
  files were found; matches were limited to documented placeholder values and
  variable names.
- Confirmed `.env.temporal` and `firmware/.env.wifi` are ignored and have never
  appeared in the committed filename history. Hardened `.gitignore` with
  generic dotenv, private-key, certificate-bundle, and credentials-file rules
  while explicitly retaining sanitized `.example` templates.

## 2026-08-17 — Code-quality and durability review

- Ran OpenCodeReview 1.9.5 against 20 first-party files using GPT-5.4 through
  a process-only API key. The first attempt exposed an OCR provider-mode bug:
  `OCR_USE_ANTHROPIC=false` is treated as truthy; `0` selects OpenAI, while the
  native `--provider openai` path is the reliable configuration. Vendored SDK
  code and the large upstream trivia JSON were deliberately excluded.
- Removed the duplicated firmware/web game model and introduced the small
  `temporal-trivia-shared` crate. It owns the serialized contract, rejects
  out-of-range answer indices while deserializing, preserves the intentional
  pre-extension deadline default, and sorts tied winner callsigns explicitly.
- Replaced plaintext `cargo:rustc-env` credential directives with an ignored
  generated Rust config under `target/`. Firmware and controller now share a
  tested dotenv parser; explicit config-file overrides fail on missing paths.
  Credentials remain embedded in the flash image by design but no longer
  appear in verbose Cargo directives.
- Made NVS state updates transactional: the cached session changes only after
  flash persistence succeeds. Corrupt state now degrades to an empty session
  and is overwritten when the next game begins. A same-game deadline can move
  forward without clearing the badge's abandoned-question set.
- Badge result watchers now have owned task handles. A new game aborts the old
  watcher, and a completed watcher can be replaced, preventing stale rounds
  from overwriting the OLED. The local monotonic ceiling is 120 seconds, safely
  beyond Temporal's 95-second Activity timeout while still bounding a stale
  build-time clock fallback.
- Question generation now validates the full authored catalog before use,
  rejects undersized deck results, and removes panic-based shuffling. The web
  observer polls at 250 ms during healthy rounds and backs off to four seconds
  during repeated query failures; SSE lag is visible in logs. Round history
  scans 100 Memo-bearing executions, sorts locally by close time, and returns
  the newest 12 because this Cloud namespace previously rejected server-side
  `ORDER BY`.
- Review follow-up fixed all four medium findings from the first diff pass.
  A focused scan of the five files skipped by the token budget produced two
  high and six medium suggestions; local history sorting was accepted, while
  fixed-array, one-time startup I/O, intentional no-duplicate prompt filtering,
  serialized NVS access, and extension-deadline semantics were verified as
  context-dependent false positives or accepted embedded tradeoffs.
- Final host gates: strict Clippy passes with `-D warnings`; 11 tests pass
  across shared and web crates; formatting, shell syntax, and diff whitespace
  checks pass. The ESP32-S3 release firmware rebuilt twice in 2m13s and 2m12s;
  the final build completed without warnings. Physical behavior was not
  reflashed in this review because the firmware changes are structural and the
  target release build is the relevant gate; prior live badge validation still
  stands.
- After the focused follow-up renamed the extension deadline helper, the final
  ESP32-S3 release build passed again in 2m10s with no warnings. The supervised
  controller restarted as PID 40373, restored the frozen Cloud result, and its
  history endpoint returned the four Memo-bearing rounds newest-first.

## 2026-08-17 — Post-review badge flash validation

- Flashed the reviewed release ELF to the connected ESP32-S3 with
  `./flash-badge.sh`. Espflash identified the expected 16 MiB device and wrote
  the 8,041,616-byte application into the explicit 14,680,064-byte factory
  partition (54.78%).
- The physical badge completed a clean boot from the new image. The bootloader
  found 16 MiB flash, the 8 MiB PSRAM memory test passed, and the application
  retained its stable `KEEN-SEAL-70` callsign.
- Wi-Fi reconnected to the configured travel-router network. The Worker then
  logged that it was polling `temporal-trivia-badges-v1`, closing the
  post-review physical validation gate.
- Live play immediately exposed that the nominal 30/15/15/40 category mix was
  appended as four contiguous runs, so a normal round began with all 30 Rust
  questions. Each weighted batch is now shuffled before scheduling while
  retaining its exact category counts and unique-question guarantee. A
  regression test verifies that the opening questions mix at least three
  categories for the fixed test seed; the full host suite now passes 12 tests
  with strict Clippy warnings enabled.
- Clarified the hidden operator entry point in the README: click the `TP7` PCB
  test pad or press `O` to open the durable Workflow Signal controls.
- Exercised the supervised Mac crash endpoint during a live Cloud round. The
  controller process exited, `run-web.sh` restarted it with a new PID, and the
  state endpoint recovered the same active Workflow with its player, score,
  latest answer, double-points window, and used 30-second extension intact.

## 2026-08-17 — Visible Mac Worker recovery

- Replaced the single generic offline message with a four-stage recovery view:
  Worker stopped, supervisor restart, Temporal reconnect, and History restored.
  The scoreboard remains dimly visible behind it so the audience can see that
  the race state is frozen rather than reset.
- The browser begins the sequence as soon as the crash endpoint acknowledges
  the command. EventSource reconnection and a successful state refresh are the
  gate for the final recovered state; it remains visible for 2.5 seconds before
  returning to the race.
- Verified the sequence against the real supervised controller in the in-app
  browser. The restart stage rendered clearly at the mirrored 16:9 layout, the
  Rust process restarted, and the frozen winner returned unchanged after
  Temporal replay.

## 2026-08-17 — Component-owned setup guides

- Split the deployment documentation by component. `firmware/README.md` now
  owns ESP Rust installation, embedded configuration, release builds, serial
  discovery, flashing, controls, and physical verification. It explicitly
  targets the Temporal Replay 2026 Badge rather than a generic ESP32-S3 board.
- `web/README.md` now owns Temporal controller configuration, supervised server
  startup, scoreboard operation, Workflow Signal controls, optional Search
  Attributes, and host tests.
- Reduced the root README to shared architecture, requirements, Temporal Cloud
  configuration, repository navigation, credential hygiene, and common checks.
  It links directly into both component guides so a web-only user is no longer
  led through firmware flashing first.

## 2026-08-17 — Unified four-button badge UI

- Replaced the plain-text waiting, connection, feedback, simulated-crash,
  recovery, sleep, and result screens with the same callsign header, central
  message area, and four framed directional cells used by questions.
- Waiting now exposes polling, Cloud, `L+R` crash, and `DOWN` sleep context.
  Result cells show score, place, correct answers, and wrong answers without
  changing the established directional icon language. Connection screens use
  the four cells as badge, Wi-Fi, Cloud, and Worker stages with a double-line
  marker on the active stage.
- Added dedicated four-cell sleep countdown and sleeping screens, factored the
  repeated panel drawing into one renderer, and added the missing `+` glyph so
  positive score feedback no longer renders as an unknown character.
- The ESP32-S3 release build passed in 2m16s with no warnings. Flashed the
  8,043,744-byte image to the connected Replay 2026 badge; 16 MiB flash and
  8 MiB PSRAM initialized, Wi-Fi reconnected, and the Worker resumed polling
  `temporal-trivia-badges-v1`. Serial validation cannot judge the subjective
  OLED spacing, so the physical waiting screen remains the visual approval
  gate.

## 2026-08-17 — Instruction-first passive badge screens

- Physical review showed that reusing framed answer cells on waiting and result
  screens made passive status look selectable. Kept the four framed directional
  cells only on real question screens and retained the centered typography for
  every other state.
- Waiting now labels actual controls explicitly: `ANSWER: PRESS DIRECTION`,
  `CRASH: HOLD LEFT+RIGHT`, and `SLEEP: HOLD DOWN 3 SEC`. Feedback, crash,
  recovery, and sleep screens use short centered explanations instead of
  decorative pseudo-buttons. Results use centered winner, score, place, and
  right/wrong totals.
- The revised ESP32-S3 release build passed in 2m10s with no warnings. Flashed
  the 8,043,984-byte image to the Replay 2026 badge; PSRAM, Wi-Fi, and Temporal
  Task Queue polling passed again.

## 2026-08-17 — Restrained game haptics

- Used the original Replay 2026 Echo firmware as the hardware and feel
  reference: GPIO 6, 80 Hz PWM, 155/255 default strength, and 35 ms standard
  pulses. The original shutdown has no dedicated pattern; its global button
  repeat buzzes every 110 ms and accelerates to every 55 ms. Kept its
  `3 -> 2 -> 1 -> 0` countdown motion but emitted only one pulse per number.
- Added distinct patterns only for meaningful state transitions: one standard
  pulse for correct, two softer pulses for wrong, one firm 120 ms pulse for a
  simulated crash, two standard pulses for recovery, three rising pulses for
  a win including ties, and one neutral pulse for other round results. Boot,
  wake, routine input, networking, and Task Queue polling remain silent.
- Centralized all motor access behind one async mutex so patterns cannot
  overlap. Each energized pulse has a drop guard that forces GPIO duty back to
  zero if its async task is cancelled, and the sleep path explicitly shuts the
  motor off before entering deep sleep.
- The host suite passed 12/12 tests. Two ESP32-S3 release builds passed in 2m15s
  and 1m47s; the final 8,061,440-byte image occupied 54.91% of the application
  partition. The connected badge booted with 16 MiB flash and 8 MiB PSRAM,
  joined Wi-Fi, synchronized time, and resumed polling
  `temporal-trivia-badges-v1` as `KEEN-SEAL-70`. Serial cannot validate tactile
  quality, so the pulse strength and rhythm still require a hand test.

## 2026-08-18 — PCB race-board redesign review

- Reviewed the user-authored web redesign as a complete replacement rather
  than an incremental styling pass. It adds a header timer/counter band,
  routed-trace score bars, a last-answer and durable-event rail, a compact
  bottom operator tray, and a dedicated finished-round summary while keeping
  stable lane positions and the badge-derived substrate, gold, and silkscreen
  palette.
- Split the astronaut, orbit rings, Space Grotesk, and Space Mono out of the
  HTML data URLs. The Rust binary still embeds each file, but serves it from a
  typed `/assets/...` route so the source remains readable. Changed their
  cache policy from one-year `immutable` caching to `no-cache` because the
  stable route names are not content hashed and would otherwise retain stale
  art after a new binary is deployed.
- A real supervised Mac Worker crash exposed a restoration race. EventSource
  could reconnect to the new process before its Temporal query completed,
  briefly replacing the frozen race with an empty waiting state and declaring
  recovery too early. The Workflow Worker and restoration query now start
  concurrently, while the HTTP listener waits for the durable snapshot. A
  repeated physical Cloud test preserved the same finished lane and winner
  throughout restart, then reported `History replayed` without the empty-state
  flash.
- Host tests pass 12/12 and strict Clippy is clean. Inline JavaScript parses;
  the four asset routes return the expected SVG/TTF content types. Browser
  checks at 1920x1080 covered waiting, live, finished, operator, and recovery
  states with no console errors or overflow. A 1400x700 viewport centered a
  1244x700 stage, and an isolated eight-badge fixture produced two equal
  four-row columns plus the bottom detail band without overflow.

## 2026-08-19 — Temporal-authentic game semantics

- Revisited the demo from the Temporal history outward. Wrong answers were
  previously sent through a Workflow Signal and then failed retryably, making
  ordinary gameplay look like infrastructure failure. They now return as
  successful Activity results, score `-1` (or `-2`), consume the question, and
  never retry. Heartbeat timeout is now the only game-generated retry path.
- Added the real `ActivityContext.info().attempt` to badge-start telemetry.
  The Workflow deduplicates question/attempt pairs, records total Activity
  attempts and heartbeat timeouts, and publishes `BADGE LOST` followed by
  `WORK REASSIGNED`, the exact attempt number, and both callsigns. The badge
  reports an attempt before consulting its abandoned-question NVS set so
  retries refused by the same Worker still count as real Temporal attempts.
- Replaced fire-and-forget chaos Signals with typed Workflow Updates and a
  validator. Double points, Rust only, and sudden death cannot overlap; the
  one-time 30-second extension remains independent. A live Cloud round accepted
  double points and rejected Rust only with `double points is already active;
  gameplay modifiers cannot overlap`.
- Added evidence-backed Mac Worker recovery. The browser no longer advances
  process, reconnect, or restoration stages with guessed timers. The server
  exposes process ID, a per-process UUID, successful-query evidence, and SHA-256
  digests for restored/current Workflow snapshots. In a live test, PID 81846
  became 82703 and the restored digest exactly matched the frozen digest
  `5be76a700ef1a648c1ecf163aa4d3bea79ac1b99cef053e16ebc719949161232`.
- The hidden TP7 tray now resolves the exact Workflow ID and Run ID and opens
  that execution in Temporal Cloud. Final results lead with the winner and use
  compact proof counters for completed Activities, heartbeat timeouts,
  reassignments, and total attempts. Raw Visibility/history remains operator
  material.
- Reduced optional Search Attributes to the agreed story fields:
  `TriviaGameStatus`, `TriviaBadgeCount`, `TriviaReassignments`,
  `TriviaWinner`, and `TriviaRustSdk`. Both the copied repo credentials and the
  existing Temporal CLI `cloud` profile returned `Request unauthorized` for
  namespace operator commands, so these remain optional and unregistered.
- Repo-local `.env` is now the first credential source for both Workers, with
  `.env.temporal` and the old TrafficLight file retained as fallbacks. The
  populated file is mode 0600 and ignored by Git.
- Host tests passed 16/16 and strict Clippy passed before the final firmware
  rebuild. The first attempt-aware 8,069,264-byte image booted on
  `KEEN-SEAL-70`, detected 16 MiB flash and 8 MiB PSRAM, joined `Shy-Fi`, and
  polled `temporal-trivia-badges-v1`. A final rebuild was required after moving
  attempt telemetry ahead of the NVS abandonment check; two-badge handoff
  validation remains pending until a second serial device is connected.
- The final release rebuild passed in 2m16s. Its 8,068,544-byte application
  occupies 54.96% of the factory partition. Reflashed `KEEN-SEAL-70`; the
  final ELF SHA prefix `9c75de5c1` booted, found 8 MiB PSRAM, joined Wi-Fi,
  connected to Temporal Cloud, and resumed polling the shared Task Queue.
- Re-ran the complete host suite after the compatibility and recovery tests:
  16/16 passed, strict Clippy passed, inline JavaScript parsed, and
  `git diff --check` passed. The final Rust controller remains live on port
  3000. After a 30-second device poll, macOS still exposed only
  `/dev/cu.usbmodem1133401`, so the agreed two-badge handoff gate is blocked on
  connecting the second physical badge rather than on software or credentials.

## 2026-08-19 — Two-badge physical failover

- Flashed the same 8,068,544-byte image to a second badge at
  `/dev/cu.usbmodem1132401`. It booted as `KEEN-RAVEN-C8`, joined `Shy-Fi` at
  `192.168.1.79`, connected to Temporal Cloud, and polled the shared Activity
  Task Queue alongside `KEEN-SEAL-70`.
- The first normal-backlog test was invalid for handoff: both one-slot Workers
  already held separate Activities, so neither could pick up the failed
  Activity before the round deadline. `KEEN-RAVEN-C8` emitted two genuine fake
  crash Signals, proving the 500 ms chord and heartbeat blackout, but the
  retries stayed queued. Its OLED went blank after the exercise; a USB reset
  restored a clean boot and display initialization. The serial log did not
  show a crash or deep-sleep cause, so the blank-display cause remains
  unresolved rather than attributed to Temporal.
- Repeated the acceptance test with `backlog_override: 1`, leaving one Worker
  intentionally free. A USB reset removed `KEEN-SEAL-70` while it owned
  `rust-030` at attempt 1. Temporal's five-second heartbeat timeout moved that
  same Activity to `KEEN-RAVEN-C8` at attempt 2 in under four seconds. The
  Workflow recorded one heartbeat timeout, one reassignment, and two attempts.
- That real reset exposed an observability flaw: the Workflow required a
  best-effort `panic_event` Signal before labeling a later badge start as a
  reassignment. Failed hardware cannot be trusted to send a final Signal. The
  Workflow now treats Temporal's increasing Activity attempt number plus a
  changed Worker identity as authoritative and uses the panic Signal only for
  earlier presentation. The regression test brought the host suite to 17/17.
- Found one stale OLED string from the old scoring model: a wrong answer still
  said `QUESTION WILL RETRY`. Changed it to `WRONG ANSWER` and
  `ACTIVITY COMPLETED` so the badge agrees with the successful Activity result
  visible in Temporal history.
- The later blank OLED on `KEEN-RAVEN-C8` was physical, not a firmware sleep
  failure: reseating the display ribbon cable restored the screen, and the
  badge then worked as expected. The earlier missing USB observation did not
  establish a software cause and should not be used as evidence of one.
- Final two-badge dress rehearsal passed as expected with both physical badges
  running the release firmware against the live Rust controller and Temporal
  Cloud. This closes the physical gameplay acceptance gate; the user approved
  publishing the resulting game changes to GitHub.

## 2026-08-20 — Reserve capacity for visible handoffs

- A two-badge round with the default ten-Activity backlog did not visibly hand
  off a crashed question. Both Workers already held Activities and eight fresh
  questions were queued ahead of the retry, so the crash alone could not
  produce a reassignment before another Worker claimed the retried attempt.
- Temporal Cloud's Activity Task Queue description returned the two live badge
  identities, `esp32-e83dc1f94bc8` and `esp32-e83dc1f94c70`, with recent poll
  timestamps. The controller now uses that server-side poller list at round
  start and schedules `max(1, badge_count - 1)` Activities, reserving one badge
  for heartbeat-timeout recovery. Explicit API backlog overrides remain for
  diagnostics.

## 2026-08-20 — Make physical badges explicit in Temporal Workers

- `temporal worker list` proved both badges were already registered as real
  Rust SDK Workers with `WORKER_STATUS_RUNNING`, Activity slot telemetry, and
  the shared Task Queue. Their identities were opaque MAC-derived `esp32-*`
  strings and the default heartbeat arrived every 60 seconds, making them easy
  to miss or mistake for stale Workers in the Temporal UI.
- Firmware now registers the Worker as `badge/CALLSIGN` while retaining the
  MAC-derived ID in game payloads and NVS. Worker heartbeat cadence is 10
  seconds so live physical devices remain visibly fresh. The controller's
  badge-count query accepts both the new identity and the old identity during
  rolling reflashes.
- The Workflow-detail Workers tab in Temporal UI is scoped to the Workflow's
  Task Queue. The Mac used `temporal-trivia-web-v1` while badges used
  `temporal-trivia-badges-v1`, so that screen correctly showed only Mac
  processes even though the Workers API contained both badges. The controller
  now polls Workflow Tasks on the badge Task Queue; task-type restrictions
  still keep Workflow execution on the Mac and Activity execution on badges,
  while future Workflow runs can show all three kinds of evidence together.

## 2026-08-20 — Rust SDK 0.7.0 and durable badge power-ups

- The live crates.io registry reported `temporalio-sdk 0.7.0`; the project was
  still pinned to `0.5.0`, and the indexed official quickstart and release page
  still showed `0.5.0`. Upgraded all Temporal Rust crates together to `0.7.0`
  and carried the existing ESP-IDF hostname, portable-atomic, and Tokio feature
  patches into freshly vendored `0.7.0` sources.
- SDK 0.7 changed Worker construction to require the high-level `Runtime`,
  removed explicit Worker task-type selection, made Activity heartbeats typed
  and async, made TLS options non-exhaustive, and introduced typed Memo and
  Search Attribute updates. The controller and firmware now use those APIs.
- The first ESP32 build reached an Xtensa LLVM backend crash in the new client
  JSON-history helper: `Cannot select ... [2 x float] [float -1.0, float
  1.0]` while compiling Serde's `ContentRefDeserializer::deserialize_float`.
  Badges never import Workflow history JSON, so the vendored client excludes
  only `WorkflowHistory::from_json` on ESP-IDF. Host builds retain the API.
- Power-up clicks remain validated Workflow Updates and now write a monotonic
  `PowerupNotice` into durable Workflow state. Each awake badge queries that
  state directly through Temporal, vibrates, displays a 1.5-second OLED
  overlay, suppresses answer input, and restores the question or idle screen.
- Host verification passed 19/19 tests and strict warnings-as-errors Clippy.
  The ESP32 release build passed and produced an 8,370,384-byte app using
  57.02% of the 14,680,064-byte factory partition. Both physical badges were
  flashed and Temporal Cloud reported `badge/KEEN-RAVEN-C8` and
  `badge/KEEN-SEAL-70` as running `temporal-rust 0.7.0` Workers with one
  Activity slot each. The Mac Workflow Worker appeared on the same Task Queue
  with SDK `0.7.0`.
- In a live round, a Rust-only Workflow Update wrote power-up sequence 1 and
  Raven logged `Displayed Temporal power-up RustOnly sequence 1` only after the
  OLED write succeeded. Seal runs the identical image; its serial monitor was
  silent during this check, so its individual overlay was not independently
  observed.
- A final offline ESP32 release build passed without warnings after cfg-gating
  the SDK's desktop-only environment detector. The resulting ELF SHA-256 is
  `0ff514959c6f800659fa5ed3429ce9e6afad99006599aeee3c21536d6160fbba`.

## 2026-08-20 — Ten-badge software dress rehearsal

- Added a Mac-side Rust badge simulator that uses the same Cloud namespace,
  `temporal-trivia-badges-v1` Task Queue, `trivia.answer_question` Activity,
  game Signals, Worker identity convention, and one-Activity-slot limit as the
  firmware. `./simulate-badges.sh 10` launches identities `badge/SIM-01`
  through `badge/SIM-10` as separate processes.
- The first design tried to register ten overlapping Activity Workers in one
  process. SDK 0.7.0 rejected both a shared Runtime and ten separate Runtimes
  with `Registration of multiple workers with overlapping worker task types on
  the same namespace, task queue, and deployment build ID not allowed`.
  Separate processes match the real deployment boundary and avoid the guard.
- One simulated process received `Connection reset by peer (os error 54)` from
  Cloud during initial connection. The launcher now restarts an individual
  simulated badge after a two-second delay instead of reducing the field.
- Live round `trivia-76f062d1455c4548babd0b71f74648ff` completed 460 of 469
  scheduled Activities in one minute with all ten simulated badges scoring.
  `SIM-05` and `SIM-10` tied for first at 57 correct answers each.
- The connected physical `KEEN-RAVEN-C8` became an eleventh visible player and
  held one Activity without answering. Temporal recorded a heartbeat timeout
  and reassigned question `rust-016` to `SIM-03` on attempt 2, producing one
  real handoff during the scale test.

## 2026-08-20 — Booth hierarchy and responsive badge feedback

- Incorporated review notes from a 16:9 finished-round screenshot without
  changing the established PCB palette or board structure. Removed the via
  circle from the lane divider, widened and centered the Winner/Tied plate,
  aligned the score to the plate's vertical center, lowered the routed score
  bar, made callsigns gray/gold, made retry telemetry gold, and kept settled
  scores white.
- New Round now enters a local three-part attract loop instead of immediately
  creating another Workflow. Seven-second panels explain that badges are Rust
  Workers, questions are Temporal Activities, missed heartbeats cause durable
  retry, and the game lasts 60 seconds. The operator still deliberately starts
  the next Workflow.
- The perceived badge freeze at Activity start had a real ordering cause: the
  firmware awaited the best-effort `badge_started` Signal before drawing a
  question it already had. It now draws first and gives start, panic, and
  recovery Signals a 750 ms ceiling. Panic feedback also appears before its
  Signal round trip. The idle OLED now says `POLLING TEMPORAL` and
  `NEXT QUESTION AUTO` so a quiet Task Queue does not look hung.
- Host tests passed 14/14, strict Clippy passed, inline JavaScript parsed,
  shell syntax and `git diff --check` passed, and the ESP32 release build
  completed in 2m37s. Browser checks at 1920x1080 measured the Winner plate and
  score at the same Y center, a 138 px plate, an 11 px name-to-bar gap, no
  divider pseudo-element, and a white settled score. The attract loop advanced
  from panel 1 to panel 2 after seven seconds, and a live ten-simulator round
  populated all ten lanes within two seconds.
- No `/dev/cu.usbmodem*` device was connected, so the new firmware could not be
  flashed or judged on physical button-to-pixel latency in this pass. Hardware
  responsiveness remains an explicit validation gate rather than a claimed
  result.
- Follow-up alignment moved each score to the full lane's vertical center and
  balanced the callsign and settled telemetry around it. At 720p, the measured
  score center was Y=148 and the midpoint between those two text rows was
  Y=149.
- The finished wide summary now lays winner and stats side by side instead of
  stacking a two-row stats grid below the available bottom band. Automated
  bounds checks found no frame escapes in running, finished, or intermission
  states at both 1920x1080 and the smaller 16:9 browser viewport.
- Simulated badges now choose a deterministic wrong answer on 20% of their
  Activities. Live round `trivia-148a369208e14ac89db2ef81d04e8812`
  completed 499 Activities with 398 correct and 101 wrong answers. The settled
  board displayed every nonzero retry count in PCB gold with no containment
  failures.
- Winner/Tied now replaces the numeric rank in the first lane column instead
  of adding a second label beside the callsign. A page-wide vertical alignment
  audit moved the title, timer, and question/handoff counters onto one shared
  header row; their measured center spread is 0 px at 720p and 1080p. Across
  all ten lanes, rank and score centers match exactly and the callsign/status
  midpoint remains within 1.65 px of the score center.
- The same audit found the live Last Answer footer could escape the horizontal
  detail rail at 720p. The ten-badge layout now uses a compact two-line question
  treatment and vertically centered answer footer; automated checks found no
  watched-content overflow at 720p or 1080p.
- Refactored the single-file CSS around shared design tokens and low-specificity
  visual-language primitives. Mono typography, uppercase labels, numeric
  formatting, single-line truncation, the frame gutter, hairline stroke, and
  pill radius now each have one source of truth; component blocks retain only
  their distinct layout and appearance. Shared edges use logical inset,
  padding, border, margin, and text-alignment properties so the CSS expresses
  intent without paired left/right declarations.
- Browser validation caught a cascade edge case during the cleanup: the generic
  `button { font: inherit; }` rule outranked a zero-specificity shared mono
  selector, causing operator tabs to render in Space Grotesk. A grouped
  single-class button-role rule restores Space Mono without `!important` or
  selector escalation. Finished, attract, and operator states retained their
  expected typography and bounds at 720p and 1080p.
- Follow-up visual review found that centering the header's three primary boxes
  did not optically align their different font sizes. The title, timer, and
  question/handoff counters now share a CSS grid baseline instead. Browser
  bounds at 720p place their rendered bottoms within 2.29 px, accounting for
  the fonts' different descenders.
- Reproducing `TEMPORAL QUERY SUCCEEDED` exposed a recovery-only layout hazard:
  an operator tray left open during an external Worker loss compressed the
  five-row lane grid below its minimum content height. `beginRecovery()` now
  closes the tray for every recovery path and blocks reopening until recovery
  completes. With ten players, the final lane stayed within 0.04 px of the
  lane-container boundary at 720p.
- Removed the divider above the first player in each lane column while
  preserving the full-width rule below the final row. The renderer marks column
  starts from the computed row count, so the treatment remains correct when the
  connected badge count changes. During validation, an initially misplaced
  loop-index edit surfaced as `index is not defined`; startup render errors now
  use the existing toast instead of silently presenting as Worker recovery.
- Finished rounds now hold the winner board for 30 seconds and then enter the
  existing three-panel attract animation without starting another Workflow.
  Repeated finished snapshots cannot restart the timer, starting a new round
  clears stale timer state, and an active Worker recovery delays the visual
  transition until recovery completes. A timestamped browser run remained on
  the finished board at 29 seconds and showed attract panel one at 31 seconds.
- Matched the open top of the two lane columns to the same vertical rhythm as
  the lower divider rows. At 720p, the header-rule-to-first-callsign gap had
  been 30 px versus 20 px below; the board now removes that extra 10 px without
  bringing back the two short top borders.

## 2026-09-01 — Tim Bruce's PR #1 integrated

- Fetched `timjbruce/temporal-trivia-badge:updates` at `a570660` and merged its
  22 commits into current `main` as merge commit `88510e6`. GitHub reported the
  PR as dirty because it branched from `7f35c4f`, before the phone-player and
  streamlined-documentation commits.
- Resolved the shared-contract split, Workflow signal handling, controller
  startup, and README conflicts while retaining phone-only rounds, dynamic
  badge/phone Activity targets, and the 150-question History payload cap.
- Preserved Tim's SDK envconfig migration, real recovery-query health flag,
  guarded memo update, reduced Visibility upserts, shared identity derivation,
  time-aware chaos validation, and host-testable badge-input state machine.
- The combined host gate passed strict Clippy and 72 tests across `badge-input`,
  `badge-screen`, `shared`, and `web`; `git diff --check` passed after removing
  trailing spaces from the archived migration diff.
- The ESP32-S3 release build passed in 2m20s after reusing the checkout's
  ignored `ldproxy`, Wi-Fi, and Temporal environment files. No badge was
  flashed, so physical button behavior remains unvalidated for this merge.

## 2026-09-01 — Tim PR firmware test-flashed to KEEN-RAVEN-C8

- Rediscovered two connected 16 MB ESP32-S3 revision 0.2 badges before writing:
  `KEEN-RAVEN-C8` and `KEEN-SEAL-70`. Selected only `KEEN-RAVEN-C8`; the
  second badge was not flashed.
- Rebuilt the current merged checkout in 2m07s. The release ELF SHA-256 was
  `367a6cf91cddedd2ccff86e912cde8cb260e2b371fcdf9a5ba1d626bbac8db80`,
  and the ELF contains `<badge_input::ButtonState>::advance`, confirming Tim's
  new host-tested input state machine is linked into the flashed image.
- `espflash` identified the target again and wrote the factory application with
  the explicit 16 MB partition table. The application occupied
  8,395,504 / 14,680,064 bytes (57.19%).
- A reset booted the factory partition, detected 8 MB PSRAM, passed the SRAM
  memory test, identified itself as `KEEN-RAVEN-C8`, joined the configured
  Wi-Fi network, initialized time sync, and logged
  `Polling trivia queue temporal-trivia-badges-v1 as badge/KEEN-RAVEN-C8`.
- The ESP-IDF application descriptor still prints cached version/build labels
  `c04c90f-dirty` and `Aug 20 2026`, while its runtime ELF SHA prefix matches
  the newly built `367a6cf91...`. This is a metadata freshness defect, not an
  ambiguous flashed payload, but should be corrected before relying on the
  serial version label operationally.
- Serial verification proves flash, boot, PSRAM, Wi-Fi, identity, and Temporal
  polling. OLED appearance, the LEFT/RIGHT crossover fix, crash gesture,
  haptics, and sleep/wake still require physical button/display acceptance.
- Published the sanitized hardware result to PR #1 without badge MAC or private
  LAN details: https://github.com/temporal-community/durable-triva/pull/1#issuecomment-5495947556
- Pushed merge commit `88510e6171a0274b8148b7a0b7ed4c5f7aedaa1e` over SSH as a
  non-force fast-forward from `759a355` to `temporal-community/durable-triva`
  `main`. GitHub confirmed PR #1 closed and merged at that exact commit on
  2026-09-01T15:01:23Z.

## 2026-09-01 — Firmware build metadata cache fixed

- Traced the stale serial version/build labels to Cargo reusing ESP-IDF's C
  application descriptor while relinking current Rust firmware. The old label
  therefore survived even though the flashed ELF hash and Rust symbols were
  current.
- `build-firmware.sh` now writes the checkout's `git describe --always --dirty`
  result to an ignored build-metadata input copied into the native ESP-IDF
  project. `esp-idf-sys` tracks that input and rebuilds the descriptor whenever
  the Git description changes.
- Existing checkouts need one native-cache bootstrap because their old Cargo
  metadata cannot know about a newly tracked file. The first attempt omitted
  the explicit ESP32 target and removed only two host directories; the fixed
  command removed 1,678 target files / 336 MiB and rebuilt `esp-idf-sys`.
- The clean native rebuild initially failed in the restricted sandbox while
  resolving `dl.espressif.com`, PyPI, and GitHub. Repeating the same build with
  network access succeeded; no dependency versions or source files changed.
- The rebuilt ELF embeds `88510e6-dirty` and `Sep 1 2026` instead of
  `c04c90f-dirty` and `Aug 20 2026`. A post-build assertion now fails firmware
  builds unless the ELF contains the exact expected Git description.
- The final incremental release build passed in 2m01s and printed
  `verified embedded firmware version: 88510e6-dirty`. Strict Clippy and all 72
  host tests also passed. This corrected artifact was not reflashed; physical
  button, OLED, haptic, and sleep/wake acceptance remains outstanding.

## 2026-09-01 — Badge Activity heartbeat gaps fixed and flashed

- Fetched `temporal-community/durable-triva` over SSH before editing and
  confirmed local `20bdd7e` exactly matched authoritative `main`. The
  configured personal-fork `origin` was one commit behind, but was not used as
  the upstream baseline.
- A one-question hardware round repeatedly reassigned a healthy
  `KEEN-RAVEN-C8`. The controller labels any retry without a crash Signal as
  `heartbeat timeout`, so serial diagnostics and Temporal Cloud history were
  used instead of trusting that fallback text. Cloud ultimately confirmed
  `TIMEOUT_TYPE_HEARTBEAT` with a five-second Activity heartbeat timeout.
- Early hypotheses were incomplete: changing the Worker heartbeat interval,
  setting the Activity heartbeat throttle, and moving held-button suppression
  inside the input loop did not cover setup work before that loop. NVS session
  setup and an awaited `badge_started` Signal could run before the first stable
  input heartbeat; one Signal exceeded its nominal 750 ms local timeout.
- Firmware now heartbeats immediately around setup, continues heartbeating
  while held buttons are suppressed, and sends the observational
  `badge_started` Signal outside the answer-critical path. It records through
  Core for local watchdog state and also awaits a direct server heartbeat,
  because the queued Worker path alone did not reliably reach Temporal on the
  ESP32 runtime during hardware trials.
- The shared contract now keeps a one-second heartbeat interval, a 15-second
  server timeout, and a 16-second deliberate-crash blackout, with compile-time
  ordering assertions. Five seconds proved too aggressive for transient
  embedded Wi-Fi and Cloud RPC stalls. The Workflow and firmware consume the
  same constants.
- An incremental `espflash` write twice left a changed trailing application
  segment erased and produced `invalid segment length 0xffffffff`. A full
  `--no-skip` write recovered the factory partition; subsequent validation
  flashes used the same full-write mode and booted all six app segments.
- Final dirty-build acceptance on `KEEN-RAVEN-C8` reported Activity heartbeat
  timeout `Some(15s)`, held attempt 1 for the complete 60-second round, and
  finished with zero reassignments and zero heartbeat timeouts. The second
  connected badge was not flashed.
- Strict Clippy, all 72 host tests, `git diff --check`, the ESP32-S3 release
  build, bootloader segment validation, 8 MB PSRAM test, Wi-Fi connection, and
  Temporal queue polling passed. No answer was recorded during the hands-on
  prompt, so button choice, crash gesture, haptics, and sleep/wake remain
  physically unconfirmed.

## 2026-09-01 — Clean-commit button acceptance

- Rebuilt the committed heartbeat fix as clean firmware version `100db4c`,
  performed a full `--no-skip` flash to `KEEN-RAVEN-C8`, and again verified all
  six application segments, PSRAM, Wi-Fi, and Temporal queue polling.
- A subsequent hands-on press on Raven was recorded by the Workflow and
  completed its Activity, proving the flashed button-to-Temporal answer path.
  The answer was scored wrong (`-1`), matching the operator's confirmation that
  they intentionally pressed a wrong answer. Crash gesture, haptics, and
  sleep/wake remain physically unconfirmed.

## 2026-09-01 — Two-badge USB hardware-in-the-loop acceptance

- A live round exposed a recovery-reserve UI defect: Raven retained the prior
  win screen until Seal answered in the next round. The result watcher exited
  after rendering final standings, so a badge that did not immediately receive
  another Activity had no path back to an idle screen.
- Firmware now holds final standings for five seconds and restores the waiting
  screen. A newly assigned Activity aborts the prior result watcher, preventing
  that delayed restore from overwriting a newer question.
- Added a USB-local HIL protocol to the physical firmware. `HIL STATUS` reports
  the stable callsign and current Activity state; `HIL ANSWER CORRECT` reads the
  correct index from the question actually received by that badge Worker and
  injects the corresponding press/release gesture into the same `ButtonState`
  path used by the GPIO buttons. Explicit indexes `0` through `3` are also
  available for directional mapping tests. Answer injection is rejected unless
  the badge currently owns a question.
- Added `tools/test_physical_badges.py`, a typed PEP 723 `uv` script that owns
  both USB serial connections, identifies exactly two physical badges, starts a
  two-question Workflow, waits for both Workers, requests correct answers, and
  requires firmware acknowledgements, input logs, Temporal scoring, Workflow
  completion, and return-to-waiting logs from both boards.
- Full `--no-skip` flashes of the dirty HIL build succeeded on
  `KEEN-RAVEN-C8` and `KEEN-SEAL-70`. Both ESP32-S3 boards booted all six
  application segments, passed the 8 MB PSRAM check, connected to Wi-Fi, and
  polled the Temporal badge queue.
- The first HIL run was manually interrupted after both correct answers and
  Seal's waiting restore while Raven was still in its result hold. A complete
  second run, Workflow `trivia-0b14b34fb44b46e18f698c75205e2aa1`, recorded
  one correct answer from each physical callsign and ended with both badges
  logging their return to waiting. The runner printed
  `PASS: both physical badges answered correctly and returned to waiting`.
- Strict Clippy, all 73 host tests, Ruff lint and format checks, and the ESP32-S3
  release build passed. This proves the real boards' boot, PSRAM, Wi-Fi,
  question ownership, firmware input state machine, Activity completion,
  scoring, and post-result readiness. It does not optically inspect OLED pixels
  or physically confirm haptic strength, face-button mechanics, sleep/wake, or
  the deliberate crash gesture.

## 2026-09-01 — Every connected badge gets a question

- A normal two-badge round assigned only Seal while Raven waited. This was not
  a connection failure: the Workflow deliberately subtracted one from the
  detected badge count to keep a recovery reserve, so two badges produced only
  one outstanding question Activity.
- The desired player experience is for every connected badge to keep playing.
  The default backlog now has one outstanding badge Activity per badge detected
  at round start. Registered phones likewise receive one slot each; a
  phone-only round retains one bootstrap Activity before the first phone joins.
- The tradeoff is explicit: a heartbeat retry may wait until a Worker becomes
  available instead of idling one healthy player for the entire normal round.
  The diagnostic backlog override remains available.
- Updated the game contract, controller guide, event text, shared backlog test,
  and Workflow target test. The running controller still needs to be rebuilt
  and restarted before live acceptance of this scheduler change.
- Tightened the physical HIL runner so normal acceptance supplies no backlog
  override and requires both badge Workers to hold questions simultaneously
  before either answer is injected. The earlier sequential answer loop could
  have passed even with a one-slot scheduler by freeing work for the other
  badge. An explicit override remains available only as a diagnostic option.
- The first no-override hardware run passed simultaneous ownership, correct
  scoring, immediate follow-up assignment, and return to waiting on both
  badges. Raven's first question arrived on Activity attempt 2 because the HIL
  thread answered `STATUS` before its Temporal Worker had begun polling. The
  runner now waits for polling logs from both physical Workers before starting
  the round, matching the operator instruction to start only after badges are
  ready.
- During that reconnect, both badges briefly logged a nondeterminism error while
  their power-up monitor queried the previous completed round, whose history
  was written with the old one-slot scheduler. The new round itself used the
  new code and completed normally. This is a rollout-compatibility warning for
  the immediately preceding history, not a failure of the new round; a second
  run against the new latest history is required to confirm the warning clears.
- The readiness-corrected second run, Workflow
  `trivia-8ddab9f5ad964c9694f5be46c2e16c39`, started only after both boards logged
  Temporal polling. Raven and Seal then received attempt-1 questions
  simultaneously, each scored one correct answer, each received another
  attempt-1 question immediately, and both returned to waiting after results.
  The runner printed `PASS`, and the old-history nondeterminism warning did not
  recur against the new latest Workflow history.

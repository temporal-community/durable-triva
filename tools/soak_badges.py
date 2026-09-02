#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = [
#   "pyserial>=3.5,<4",
# ]
# ///

"""Soak two physical badges over USB and hunt for firmware faults.

`test_physical_badges.py` proves one good round. This plays badly on purpose,
for as long as you let it: answers at human pace, injects the crash gesture,
and sits idle between rounds -- the three windows every fault found so far has
appeared in. It watches both serial streams for panics, aborts, reboots and
silence, decodes any backtrace against a snapshot of the flashed ELF, and
keeps going, so a fault that needs twenty rounds to surface still gets found.

Needs badges flashed with `./build-firmware.sh --features hil`.
"""

from __future__ import annotations

import argparse
import random
import re
import subprocess
import time
from pathlib import Path
from typing import cast

from badge_serial import (
    BadgePort,
    Fault,
    discover_ports,
    game_status,
    listed_badges,
    request_json,
    wait_for_status,
)

FIRMWARE_ELF = "target/xtensa-esp32s3-espidf/release/temporal-trivia-badge-firmware"
# Any of these means the firmware stopped being the firmware. The trailing
# reset pattern deliberately excludes USB_UART_CHIP_RESET, which is just a
# host opening the port.
FAULT_PATTERNS = (
    r"Guru Meditation",
    r"Debug exception reason",
    r"Stack canary watchpoint",
    r"StackOverflow",
    r"abort\(\) was called",
    r"assert failed",
    r"Brownout",
    r"rst:0x[0-9a-f]+ \((?!USB_UART_CHIP_RESET)",
)
# A badge whose Worker exits stops saying anything at all. Until this counted,
# the soak sat waiting on a dead board for ten minutes and reported nothing.
SILENCE_IS_A_FAULT = 90.0
# A person reads the prompt, reads four answers, decides, then presses. Nobody
# does that in 300 ms, and hammering at machine speed tests a regime the demo
# never runs in: far more Activities per round than a round can really produce.
THINK_SECONDS = (1.8, 6.5)


def decode_backtrace(fault: Fault, elf: Path) -> None:
    """Turn any captured backtrace addresses into source locations."""
    addresses: list[str] = []
    for line in fault.context:
        if "Backtrace:" in line:
            addresses.extend(re.findall(r"0x4[0-9a-f]{7}", line))
    if not addresses:
        return
    try:
        result = subprocess.run(
            ["xtensa-esp32s3-elf-addr2line", "-pfiaC", "-e", str(elf), *addresses],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        fault.decoded = [f"could not decode: {error}"]
        return
    fault.decoded = [line for line in result.stdout.splitlines() if line.strip()]


def play_round(
    badges: list[BadgePort],
    controller: str,
    rng: random.Random,
    crash_chance: float,
    timeout: float,
) -> int:
    """Play one round at human pace. Returns how many inputs were sent."""
    deadline = time.monotonic() + timeout
    # Per badge: the question it is looking at, and when it will act on it.
    looking_at: dict[str, str] = {}
    act_at: dict[str, float] = {}
    handled: set[str] = set()
    inputs = 0

    while time.monotonic() < deadline:
        if game_status(controller) == "finished":
            return inputs
        for badge in badges:
            badge.check_alive()
            match = badge.status(timeout=3.0)
            if match is None:
                continue
            if match.group("active") != "true":
                looking_at.pop(badge.name, None)
                continue
            question = match.group("question")
            key = f"{badge.name}:{question}"
            if key in handled:
                continue
            if looking_at.get(badge.name) != question:
                # A new question just appeared. Start reading it.
                looking_at[badge.name] = question
                act_at[badge.name] = time.monotonic() + rng.uniform(*THINK_SECONDS)
                continue
            if time.monotonic() < act_at[badge.name]:
                continue
            handled.add(key)
            inputs += 1
            if rng.random() < crash_chance:
                badge.send("HIL CRASH")
            else:
                # A wrong answer is as good a test as a right one, so pick
                # freely rather than always correct.
                badge.send(
                    "HIL ANSWER CORRECT"
                    if rng.random() < 0.5
                    else f"HIL ANSWER {rng.randrange(4)}"
                )
        time.sleep(0.25)
    return inputs


def run_soak(
    ports: list[str],
    controller: str,
    rounds: int,
    idle_seconds: float,
    crash_chance: float,
    seed: int,
    log: Path,
    boot_timeout: float,
) -> int:
    """Play `rounds` rounds and report every fault. Returns an exit code."""
    rng = random.Random(seed)
    badges = [
        BadgePort(
            path=path,
            log=log,
            fault_patterns=FAULT_PATTERNS,
            silence_is_a_fault=SILENCE_IS_A_FAULT,
        )
        for path in ports
    ]
    # Snapshot the ELF under test. Rebuilding during a soak is normal, and
    # decoding a backtrace against a later binary silently invents symbols.
    elf = log.with_suffix(".elf")
    source = Path(FIRMWARE_ELF)
    if not source.is_file():
        raise RuntimeError(f"no firmware ELF at {source}; build it first")
    elf.write_bytes(source.read_bytes())
    print(f"decoding against a snapshot of {source} -> {elf}", flush=True)

    completed = 0
    aborted: Exception | None = None
    try:
        for badge in badges:
            badge.open()
        for badge in badges:
            if badge.status(timeout=boot_timeout) is None:
                raise RuntimeError(
                    f"{badge.path}: no HIL STATUS. Flash with "
                    "./build-firmware.sh --features hil"
                )
        print(f"soak: {', '.join(b.name for b in badges)}  seed={seed}", flush=True)

        for index in range(1, rounds + 1):
            for badge in badges:
                if not badge.await_polling(120.0):
                    badge.check_alive()
                    print(f"[{badge.name}] never reported polling", flush=True)
            wanted = {cast(str, badge.callsign) for badge in badges}
            deadline = time.monotonic() + 90
            while not wanted <= listed_badges(controller):
                if time.monotonic() > deadline:
                    print("controller never listed every badge", flush=True)
                    break
                time.sleep(1.0)

            if game_status(controller) != "finished":
                request_json(controller, "/api/end-round", method="POST")
                wait_for_status(controller, "finished", 30)
            started = request_json(controller, "/api/start", method="POST", payload={})
            print(f"round {index}/{rounds}: {started.get('game_id')}", flush=True)
            inputs = play_round(badges, controller, rng, crash_chance, timeout=90)
            wait_for_status(controller, "finished", 60)
            completed += 1
            print(f"  {inputs} inputs at human pace", flush=True)

            # Every fault so far has appeared either mid-round or on a badge
            # doing nothing at all, so the quiet window is part of the test.
            print(f"  idling {idle_seconds:.0f}s", flush=True)
            quiet_until = time.monotonic() + idle_seconds
            while time.monotonic() < quiet_until:
                time.sleep(1.0)
                for badge in badges:
                    badge.check_alive()
    except (RuntimeError, TypeError, TimeoutError, OSError) as error:
        # Stop the run, but never at the cost of the report.
        aborted = error
        print(f"\nsoak stopped early: {error}", flush=True)
    finally:
        for badge in badges:
            badge.close()

    faults = [fault for badge in badges for fault in badge.faults]
    for fault in faults:
        decode_backtrace(fault, elf)

    print("\n" + "=" * 72)
    if aborted is not None:
        print(f"run incomplete   : {aborted}")
    print(f"rounds completed : {completed}/{rounds}")
    for badge in badges:
        print(f"{badge.name:<16} boots={badge.boots}  faults={len(badge.faults)}")
    print(f"serial log       : {log}")
    if not faults:
        if aborted is not None:
            print("INCONCLUSIVE: no firmware faults, but the run did not finish")
            return 2
        print("PASS: no firmware faults observed")
        return 0
    print(f"\nFAIL: {len(faults)} fault(s)")
    for fault in faults:
        print("\n" + "-" * 72)
        print(f"[{fault.badge}] {fault.line}")
        for line in fault.context:
            print(f"  | {line}")
        for line in fault.decoded:
            print(f"  > {line}")
    return 1


def main() -> None:
    """Parse arguments and soak the connected badges."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", action="append", default=[], dest="ports")
    parser.add_argument("--controller", default="http://127.0.0.1:3000")
    parser.add_argument("--rounds", type=int, default=10)
    parser.add_argument("--idle-seconds", type=float, default=120.0)
    parser.add_argument("--crash-chance", type=float, default=0.15)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--log", default="/tmp/badge-soak.log")
    parser.add_argument(
        "--boot-timeout",
        type=float,
        default=150.0,
        help="how long to wait for a freshly flashed badge to answer HIL STATUS",
    )
    arguments = parser.parse_args()

    log = Path(cast(str, arguments.log))
    log.write_text("", encoding="utf-8")
    raise SystemExit(
        run_soak(
            discover_ports(cast(list[str], arguments.ports)),
            cast(str, arguments.controller),
            cast(int, arguments.rounds),
            cast(float, arguments.idle_seconds),
            cast(float, arguments.crash_chance),
            cast(int, arguments.seed),
            log,
            cast(float, arguments.boot_timeout),
        )
    )


if __name__ == "__main__":
    main()

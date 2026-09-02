#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = [
#   "pyserial>=3.5,<4",
# ]
# ///

"""Run a two-badge hardware-in-the-loop Temporal trivia acceptance test.

Proves one good round end to end: both real badge Workers holding questions at
the same moment, one correct answer each through the same input state machine
the face buttons drive, and both boards back on the waiting screen afterwards.

Needs badges flashed with `./build-firmware.sh --features hil`. For a long
adversarial run instead of a single clean one, see `soak_badges.py`.
"""

from __future__ import annotations

import argparse
import re
import time
from typing import cast

from badge_serial import (
    BadgePort,
    JsonValue,
    discover_ports,
    request_json,
    wait_for_scheduler_roster,
)


def player_correct_count(state: dict[str, JsonValue], callsign: str) -> int:
    """Extract one callsign's correct-answer count from controller state."""
    players = state.get("players")
    if not isinstance(players, dict):
        return 0
    for raw_player in players.values():
        if not isinstance(raw_player, dict):
            continue
        if raw_player.get("callsign") != callsign:
            continue
        correct = raw_player.get("correct")
        return correct if isinstance(correct, int) else 0
    return 0


def wait_for_controller_result(
    controller: str, callsigns: list[str], timeout: float
) -> dict[str, JsonValue]:
    """Wait until both physical callsigns have a correct Temporal answer."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        state = request_json(controller, "/api/state")
        if all(player_correct_count(state, callsign) >= 1 for callsign in callsigns):
            return state
        time.sleep(0.25)
    raise TimeoutError("controller did not record one correct answer from each badge")


def wait_for_finished(controller: str, timeout: float) -> None:
    """Wait for the hardware round to reach its final Workflow state."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if request_json(controller, "/api/state").get("status") == "finished":
            return
        time.sleep(0.5)
    raise TimeoutError("physical badge round did not finish")


def run_test(
    ports: list[str],
    controller: str,
    timeout: float,
    backlog_override: int | None,
) -> None:
    """Run one correct-answer round through both real badge Workers."""
    badges = [BadgePort(path=path) for path in ports]
    try:
        for badge in badges:
            badge.open(echo=True)
        callsigns = [badge.identify(timeout) for badge in badges]
        if len(set(callsigns)) != 2:
            raise RuntimeError(f"badge callsigns are not unique: {callsigns}")
        print(f"HIL identified physical badges: {', '.join(callsigns)}")

        for badge in badges:
            if not badge.await_polling(timeout):
                raise TimeoutError(f"{badge.name}: Worker never reported polling")
        print("HIL both physical badge Workers report polling")

        wait_for_scheduler_roster(controller, callsigns, timeout)
        print("HIL controller lists both badges; the round will be sized for two")

        markers = {badge.path: badge.mark() for badge in badges}
        start_payload: dict[str, JsonValue] = {}
        if backlog_override is not None:
            start_payload["backlog_override"] = backlog_override
        started = request_json(
            controller, "/api/start", method="POST", payload=start_payload
        )
        print(f"HIL started round: {started.get('game_id')}")

        for badge in badges:
            if (
                badge.wait_for(
                    r"Question .* preparation complete",
                    timeout,
                    after=markers[badge.path],
                )
                is None
            ):
                raise TimeoutError(f"{badge.name}: never received a question")
        # Reaching that log line proves each badge held a question at some
        # point, which two sequential ownerships also satisfy. Simultaneous
        # ownership is the property a one-slot scheduler would fail, so ask
        # both boards what they are holding right now.
        if not all(badge.owns_question() for badge in badges):
            raise RuntimeError(
                "badges did not hold questions simultaneously; "
                "the scheduler is not feeding every connected badge"
            )
        print("HIL both physical badges hold questions simultaneously")

        for badge in badges:
            marker = badge.mark()
            badge.send("HIL ANSWER CORRECT")
            for pattern in (r"HIL ACK answer=", r"Input selected answer="):
                if badge.wait_for(pattern, 5.0, after=marker) is None:
                    raise TimeoutError(f"{badge.name}: no {pattern!r} after answering")

        state = wait_for_controller_result(controller, callsigns, timeout)
        print(
            "HIL Temporal answers recorded: "
            + ", ".join(
                f"{callsign}={player_correct_count(state, callsign)} correct"
                for callsign in callsigns
            )
        )

        wait_for_finished(controller, timeout)
        for badge in badges:
            expected = (
                rf"Result hold complete; {re.escape(badge.name)} returned to waiting"
            )
            if badge.wait_for(expected, timeout, after=markers[badge.path]) is None:
                raise TimeoutError(f"{badge.name}: never returned to waiting")
        print("PASS: both physical badges answered correctly and returned to waiting")
    finally:
        for badge in badges:
            badge.close()


def main() -> None:
    """Parse command-line arguments and run the physical badge test."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", action="append", default=[], dest="ports")
    parser.add_argument("--controller", default="http://127.0.0.1:3000")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument(
        "--backlog-override",
        type=int,
        help="diagnostic question backlog; omitted by default to test normal policy",
    )
    arguments = parser.parse_args()
    run_test(
        discover_ports(cast(list[str], arguments.ports), expected=2),
        cast(str, arguments.controller),
        cast(float, arguments.timeout),
        cast(int | None, arguments.backlog_override),
    )


if __name__ == "__main__":
    main()

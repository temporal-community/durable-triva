#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = [
#   "pyserial>=3.5,<4",
# ]
# ///

"""Run a two-badge hardware-in-the-loop Temporal trivia acceptance test."""

from __future__ import annotations

import argparse
import json
import re
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from glob import glob
from typing import TypeAlias, cast
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

import serial

JsonValue: TypeAlias = (
    None | bool | int | float | str | list["JsonValue"] | dict[str, "JsonValue"]
)


@dataclass
class BadgePort:
    """Own one physical badge serial connection and its captured log lines."""

    path: str
    connection: serial.Serial = field(init=False)
    callsign: str | None = field(default=None, init=False)
    _lines: deque[tuple[int, str]] = field(
        default_factory=lambda: deque(maxlen=4_000), init=False
    )
    _sequence: int = field(default=0, init=False)
    _condition: threading.Condition = field(
        default_factory=threading.Condition, init=False
    )
    _stop: threading.Event = field(default_factory=threading.Event, init=False)
    _reader: threading.Thread = field(init=False)

    def open(self) -> None:
        """Open without intentionally toggling reset and begin capturing logs."""
        connection = serial.Serial()
        connection.port = self.path
        connection.baudrate = 115_200
        connection.timeout = 0.2
        connection.write_timeout = 1.0
        connection.dtr = False
        connection.rts = False
        connection.exclusive = True
        connection.open()
        self.connection = connection
        self._reader = threading.Thread(
            target=self._read_lines,
            name=f"serial-{self.path.rsplit('/', maxsplit=1)[-1]}",
            daemon=True,
        )
        self._reader.start()

    def close(self) -> None:
        """Stop capture and release the serial device."""
        self._stop.set()
        if hasattr(self, "connection"):
            self.connection.close()
        if hasattr(self, "_reader"):
            self._reader.join(timeout=1.0)

    def mark(self) -> int:
        """Return the current log sequence for later scoped waits."""
        with self._condition:
            return self._sequence

    def send(self, command: str) -> None:
        """Send one newline-terminated HIL command."""
        self.connection.write(f"{command}\n".encode())
        self.connection.flush()

    def wait_for(self, pattern: str, timeout: float, *, after: int = 0) -> str:
        """Wait for a matching serial line newer than ``after``."""
        matcher = re.compile(pattern)
        deadline = time.monotonic() + timeout
        with self._condition:
            while True:
                for sequence, line in self._lines:
                    if sequence > after and matcher.search(line):
                        return line
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"{self.callsign or self.path}: no serial match for {pattern!r}"
                    )
                self._condition.wait(timeout=min(remaining, 0.5))

    def identify(self, timeout: float) -> str:
        """Ask firmware for its stable callsign and return it."""
        deadline = time.monotonic() + timeout
        marker = self.mark()
        while time.monotonic() < deadline:
            self.send("HIL STATUS")
            try:
                line = self.wait_for(r"HIL STATUS callsign=([^ ]+)", 1.0, after=marker)
            except TimeoutError:
                continue
            match = re.search(r"callsign=([^ ]+)", line)
            if match is not None:
                self.callsign = match.group(1)
                return self.callsign
        raise TimeoutError(f"{self.path}: badge did not answer HIL STATUS")

    def _read_lines(self) -> None:
        while not self._stop.is_set():
            try:
                raw_line = self.connection.readline()
            except (OSError, serial.SerialException):
                if not self._stop.is_set():
                    time.sleep(0.1)
                continue
            if not raw_line:
                continue
            line = raw_line.decode(errors="replace").strip()
            if not line:
                continue
            with self._condition:
                self._sequence += 1
                self._lines.append((self._sequence, line))
                self._condition.notify_all()
            print(f"[{self.callsign or self.path}] {line}")


def request_json(
    controller: str,
    path: str,
    *,
    method: str = "GET",
    payload: dict[str, JsonValue] | None = None,
) -> dict[str, JsonValue]:
    """Call one controller JSON endpoint and validate its top-level shape."""
    body = json.dumps(payload).encode() if payload is not None else None
    request = Request(
        f"{controller.rstrip('/')}{path}",
        data=body,
        method=method,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urlopen(request, timeout=10) as response:
            decoded: object = json.loads(response.read())
    except (HTTPError, URLError) as error:
        raise RuntimeError(f"controller request failed: {error}") from error
    if not isinstance(decoded, dict):
        raise TypeError("controller returned a non-object JSON response")
    return cast(dict[str, JsonValue], decoded)


def discover_ports(explicit_ports: list[str]) -> list[str]:
    """Return exactly two distinct USB modem device paths."""
    ports = explicit_ports or sorted(glob("/dev/cu.usbmodem*"))
    if len(ports) != 2 or len(set(ports)) != 2:
        raise RuntimeError(
            f"expected exactly two distinct badge ports, found {len(ports)}: {ports}"
        )
    return ports


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


def wait_for_finished(controller: str, timeout: float) -> dict[str, JsonValue]:
    """Wait for the hardware round to reach its final Workflow state."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        state = request_json(controller, "/api/state")
        if state.get("status") == "finished":
            return state
        time.sleep(0.5)
    raise TimeoutError("physical badge round did not finish")


def run_test(ports: list[str], controller: str, timeout: float) -> None:
    """Run one correct-answer round through both real badge Workers."""
    badges = [BadgePort(path) for path in ports]
    try:
        for badge in badges:
            badge.open()
        callsigns = [badge.identify(timeout) for badge in badges]
        if len(set(callsigns)) != 2:
            raise RuntimeError(f"badge callsigns are not unique: {callsigns}")
        print(f"HIL identified physical badges: {', '.join(callsigns)}")

        markers = {badge.path: badge.mark() for badge in badges}
        started = request_json(
            controller,
            "/api/start",
            method="POST",
            payload={"backlog_override": len(badges)},
        )
        print(f"HIL started round: {started.get('game_id')}")

        for badge in badges:
            badge.wait_for(
                r"Question .* preparation complete",
                timeout,
                after=markers[badge.path],
            )
            marker = badge.mark()
            badge.send("HIL ANSWER CORRECT")
            badge.wait_for(r"HIL ACK answer=", 5.0, after=marker)
            badge.wait_for(r"Input selected answer=", 5.0, after=marker)

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
            badge.wait_for(
                rf"Result hold complete; {re.escape(cast(str, badge.callsign))} returned to waiting",
                timeout,
                after=markers[badge.path],
            )
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
    arguments = parser.parse_args()
    run_test(
        discover_ports(cast(list[str], arguments.ports)),
        cast(str, arguments.controller),
        cast(float, arguments.timeout),
    )


if __name__ == "__main__":
    main()

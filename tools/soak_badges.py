#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = [
#   "pyserial>=3.5,<4",
# ]
# ///

"""Soak two physical badges over USB and hunt for firmware faults.

`test_physical_badges.py` proves one good round. This plays badly on purpose,
for as long as you let it: answers the instant a question appears, injects the
crash gesture, and sits idle between rounds -- the three windows every fault
seen so far has appeared in. It watches the serial streams for panics, reboots
and silence, decodes any backtrace against the flashed ELF, and keeps going so
a fault that needs twenty rounds to show up still gets found.

Needs badges flashed with `./build-firmware.sh --features hil`.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import subprocess
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from glob import glob
from pathlib import Path
from typing import TypeAlias, cast
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

import serial

JsonValue: TypeAlias = (
    None | bool | int | float | str | list["JsonValue"] | dict[str, "JsonValue"]
)

FIRMWARE_ELF = "target/xtensa-esp32s3-espidf/release/temporal-trivia-badge-firmware"
# Any of these means the firmware stopped being the firmware.
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
BOOT_MARKER = "Temporal Trivia badge booting as"
POLLING_MARKER = "Polling trivia queue"


@dataclass
class Fault:
    """One thing that went wrong, with whatever context explains it."""

    badge: str
    kind: str
    line: str
    context: list[str]
    decoded: list[str] = field(default_factory=list)


@dataclass
class BadgePort:
    """Own one badge's serial connection, its log, and its fault detection."""

    path: str
    log: Path
    callsign: str | None = field(default=None, init=False)
    faults: list[Fault] = field(default_factory=list, init=False)
    boots: int = field(default=0, init=False)
    connection: serial.Serial = field(init=False)
    _lines: deque[tuple[int, str]] = field(
        default_factory=lambda: deque(maxlen=8_000), init=False
    )
    _recent: deque[str] = field(default_factory=lambda: deque(maxlen=40), init=False)
    _sequence: int = field(default=0, init=False)
    _condition: threading.Condition = field(
        default_factory=threading.Condition, init=False
    )
    _stop: threading.Event = field(default_factory=threading.Event, init=False)
    _last_line_at: float = field(default_factory=time.monotonic, init=False)
    _pending_fault: Fault | None = field(default=None, init=False)
    _fault_tail: int = field(default=0, init=False)

    def open(self) -> None:
        """Open without intentionally toggling reset and start capturing."""
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
        threading.Thread(
            target=self._read_lines,
            name=f"soak-{self.path.rsplit('/', maxsplit=1)[-1]}",
            daemon=True,
        ).start()

    def close(self) -> None:
        self._stop.set()
        if hasattr(self, "connection"):
            self.connection.close()

    @property
    def name(self) -> str:
        return self.callsign or self.path

    def mark(self) -> int:
        with self._condition:
            return self._sequence

    def send(self, command: str) -> None:
        try:
            self.connection.write(f"{command}\n".encode())
            self.connection.flush()
        except (OSError, serial.SerialException) as error:
            print(f"[{self.name}] serial write failed: {error}")

    def silent_for(self) -> float:
        """Seconds since this badge last said anything at all."""
        with self._condition:
            return time.monotonic() - self._last_line_at

    def wait_for(self, pattern: str, timeout: float, *, after: int = 0) -> str | None:
        """Wait for a matching line newer than ``after``; None on timeout."""
        matcher = re.compile(pattern)
        deadline = time.monotonic() + timeout
        with self._condition:
            while True:
                for sequence, line in self._lines:
                    if sequence > after and matcher.search(line):
                        return line
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return None
                self._condition.wait(timeout=min(remaining, 0.25))

    def status(self, timeout: float = 4.0) -> re.Match[str] | None:
        """One fresh HIL STATUS, or None if the badge did not answer.

        The HIL reader only starts once the Worker does, which is after Wi-Fi,
        SNTP and the Cloud connect -- roughly 25 seconds from reset. Callers
        identifying a freshly flashed badge need to allow for that.
        """
        deadline = time.monotonic() + timeout
        line = None
        while line is None and time.monotonic() < deadline:
            marker = self.mark()
            self.send("HIL STATUS")
            line = self.wait_for(r"HIL STATUS callsign=", 2.0, after=marker)
        if line is None:
            return None
        match = re.search(
            r"callsign=(?P<callsign>\S+) polling=(?P<polling>\S+) "
            r"active=(?P<active>\S+) question=(?P<question>\S+)",
            line,
        )
        if match is not None:
            self.callsign = match.group("callsign")
        return match

    def await_ready(self, timeout: float) -> bool:
        """Wait until this badge's Worker reports it is polling Temporal."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            match = self.status()
            if match is not None and match.group("polling") == "true":
                return True
            time.sleep(0.5)
        return False

    def _read_lines(self) -> None:
        with self.log.open("a", encoding="utf-8") as handle:
            while not self._stop.is_set():
                try:
                    raw = self.connection.readline()
                except (OSError, serial.SerialException):
                    if not self._stop.is_set():
                        time.sleep(0.1)
                    continue
                if not raw:
                    continue
                line = raw.decode(errors="replace").rstrip()
                if not line:
                    continue
                handle.write(f"{time.strftime('%H:%M:%S')} [{self.name}] {line}\n")
                handle.flush()
                with self._condition:
                    self._sequence += 1
                    self._last_line_at = time.monotonic()
                    self._lines.append((self._sequence, line))
                    self._recent.append(line)
                    self._note_fault(line)
                    self._condition.notify_all()

    def _note_fault(self, line: str) -> None:
        """Detect a fault and keep collecting the lines that explain it."""
        if self._pending_fault is not None:
            self._pending_fault.context.append(line)
            self._fault_tail -= 1
            if self._fault_tail <= 0:
                self.faults.append(self._pending_fault)
                self._pending_fault = None
            return
        if BOOT_MARKER in line:
            self.boots += 1
        for pattern in FAULT_PATTERNS:
            if re.search(pattern, line):
                print(f"\n!! [{self.name}] FAULT: {line}", flush=True)
                self._pending_fault = Fault(
                    badge=self.name,
                    kind=pattern,
                    line=line,
                    context=list(self._recent),
                )
                # Enough to carry a register dump and a backtrace.
                self._fault_tail = 28
                return


def decode_backtrace(fault: Fault, elf: Path) -> None:
    """Turn any captured backtrace addresses into source locations."""
    addresses: list[str] = []
    for line in fault.context:
        if "Backtrace:" not in line:
            continue
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
        with urlopen(request, timeout=15) as response:
            decoded: object = json.loads(response.read())
    except (HTTPError, URLError, TimeoutError) as error:
        raise RuntimeError(f"controller request failed: {error}") from error
    if not isinstance(decoded, dict):
        raise TypeError("controller returned a non-object JSON response")
    return cast(dict[str, JsonValue], decoded)


def listed_badges(controller: str) -> set[str]:
    roster = request_json(controller, "/api/badges").get("callsigns")
    return set(roster) if isinstance(roster, list) else set()


def game_status(controller: str) -> str:
    status = request_json(controller, "/api/state").get("status")
    return status if isinstance(status, str) else "unknown"


def wait_for_status(controller: str, wanted: str, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if game_status(controller) == wanted:
            return True
        time.sleep(0.5)
    return False


# A person reads the prompt, reads four answers, decides, then presses. Nobody
# does that in 300 ms, and hammering the badge at machine speed tests a regime
# the demo never runs in -- far more Activities per round than a round can
# really produce, and no gap for anything to settle in.
THINK_SECONDS = (1.8, 6.5)


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
    """Play `rounds` rounds and report every fault seen. Returns an exit code."""
    rng = random.Random(seed)
    badges = [BadgePort(path=path, log=log) for path in ports]
    # Snapshot the ELF under test. Rebuilding during a soak is normal, and
    # decoding a backtrace against a later binary silently invents symbols.
    elf = log.with_suffix(".elf")
    source = Path(FIRMWARE_ELF)
    if not source.is_file():
        raise RuntimeError(f"no firmware ELF at {source}; build it first")
    elf.write_bytes(source.read_bytes())
    print(f"decoding against a snapshot of {source} -> {elf}", flush=True)
    completed = 0
    try:
        for badge in badges:
            badge.open()
        for badge in badges:
            if badge.status(timeout=boot_timeout) is None:
                raise RuntimeError(
                    f"{badge.path}: no HIL STATUS. Flash with "
                    "./build-firmware.sh --features hil"
                )
        print(
            f"soak: {', '.join(badge.name for badge in badges)}  seed={seed}",
            flush=True,
        )

        for index in range(1, rounds + 1):
            for badge in badges:
                if not badge.await_ready(120.0):
                    print(f"[{badge.name}] never reported polling; continuing anyway")
            deadline = time.monotonic() + 90
            while listed_badges(controller) < {
                cast(str, badge.callsign) for badge in badges
            }:
                if time.monotonic() > deadline:
                    print("controller never listed every badge; starting regardless")
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
                    if badge.silent_for() > 240:
                        print(f"!! [{badge.name}] silent for 4 minutes")
            for badge in badges:
                if badge.faults:
                    print(f"  {badge.name}: {len(badge.faults)} fault(s) so far")
    finally:
        for badge in badges:
            badge.close()

    faults = [fault for badge in badges for fault in badge.faults]
    for fault in faults:
        decode_backtrace(fault, elf)

    print("\n" + "=" * 72)
    print(f"rounds completed : {completed}/{rounds}")
    for badge in badges:
        print(f"{badge.name:<16} boots={badge.boots}  faults={len(badge.faults)}")
    print(f"serial log       : {log}")
    if not faults:
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


def discover_ports(explicit: list[str]) -> list[str]:
    ports = explicit or sorted(glob("/dev/cu.usbmodem*"))
    if len(ports) != len(set(ports)) or not ports:
        raise RuntimeError(f"expected distinct badge ports, found {ports}")
    return ports


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

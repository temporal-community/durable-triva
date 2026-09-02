"""Shared plumbing for the tools that drive physical badges over USB.

`test_physical_badges.py` proves one good round and `soak_badges.py` hunts for
faults over many, but both own two serial ports, both parse `HIL STATUS`, and
both talk to the same controller API. They each grew their own copy of that,
and the copies had already drifted -- eleven definitions with the same names
and different bodies. This is the one copy.

Imported rather than duplicated: a PEP 723 script puts its own directory on
`sys.path`, so `import badge_serial` works and the running script's dependency
block covers the `serial` import here.
"""

from __future__ import annotations

import json
import re
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

BOOT_MARKER = "Temporal Trivia badge booting as"
STATUS_PATTERN = re.compile(
    r"callsign=(?P<callsign>\S+) polling=(?P<polling>\S+) "
    r"active=(?P<active>\S+) question=(?P<question>\S+)"
)


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
    """One badge's serial connection, its captured log, and its faults.

    Faults are collected whether or not the caller looks for them, because the
    interesting ones happen while the caller is waiting on something else.
    """

    path: str
    log: Path | None = None
    fault_patterns: tuple[str, ...] = ()
    silence_is_a_fault: float = 0.0
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
    _reported_silence: bool = field(default=False, init=False)
    _echo: bool = field(default=False, init=False)

    def open(self, *, echo: bool = False) -> None:
        """Open without intentionally toggling reset and begin capturing."""
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
        self._echo = echo
        threading.Thread(
            target=self._read_lines,
            name=f"serial-{self.path.rsplit('/', maxsplit=1)[-1]}",
            daemon=True,
        ).start()

    def close(self) -> None:
        """Stop capture and release the serial device."""
        self._stop.set()
        if hasattr(self, "connection"):
            self.connection.close()

    @property
    def name(self) -> str:
        return self.callsign or self.path

    def mark(self) -> int:
        """Return the current log sequence for later scoped waits."""
        with self._condition:
            return self._sequence

    def send(self, command: str) -> None:
        """Send one newline-terminated HIL command."""
        try:
            self.connection.write(f"{command}\n".encode())
            self.connection.flush()
        except (OSError, serial.SerialException) as error:
            print(f"[{self.name}] serial write failed: {error}", flush=True)

    def silent_for(self) -> float:
        """Seconds since this badge last said anything at all."""
        with self._condition:
            return time.monotonic() - self._last_line_at

    def check_alive(self) -> bool:
        """Record a fault if the badge has gone quiet. True while healthy."""
        if self.silence_is_a_fault <= 0:
            return True
        quiet = self.silent_for()
        if quiet < self.silence_is_a_fault:
            return True
        if self._reported_silence:
            return False
        self._reported_silence = True
        line = f"no serial output for {quiet:.0f}s -- Worker stopped or badge hung"
        print(f"\n!! [{self.name}] FAULT: {line}", flush=True)
        with self._condition:
            self.faults.append(Fault(self.name, "silence", line, list(self._recent)))
        return False

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
        match = STATUS_PATTERN.search(line)
        if match is not None:
            self.callsign = match.group("callsign")
        return match

    def identify(self, timeout: float) -> str:
        """Ask firmware for its stable callsign and return it."""
        match = self.status(timeout)
        if match is None:
            raise TimeoutError(f"{self.path}: badge did not answer HIL STATUS")
        return cast(str, match.group("callsign"))

    def await_polling(self, timeout: float) -> bool:
        """Wait until this badge's Worker reports that it is polling.

        Readiness is asked for rather than inferred from the boot log: that
        line is printed once, and these tools deliberately open the port
        without toggling reset, so on a still-running badge it scrolled past
        long ago.
        """
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            match = self.status(min(5.0, timeout))
            if match is not None and match.group("polling") == "true":
                return True
            time.sleep(0.25)
        return False

    def owns_question(self) -> bool:
        """Whether this badge is holding a question right now."""
        match = self.status(5.0)
        return match is not None and match.group("active") == "true"

    def _read_lines(self) -> None:
        handle = self.log.open("a", encoding="utf-8") if self.log else None
        try:
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
                if handle is not None:
                    handle.write(f"{time.strftime('%H:%M:%S')} [{self.name}] {line}\n")
                    handle.flush()
                if self._echo:
                    print(f"[{self.name}] {line}")
                with self._condition:
                    self._sequence += 1
                    self._last_line_at = time.monotonic()
                    self._reported_silence = False
                    self._lines.append((self._sequence, line))
                    self._recent.append(line)
                    self._note_fault(line)
                    self._condition.notify_all()
        finally:
            if handle is not None:
                handle.close()

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
        for pattern in self.fault_patterns:
            if re.search(pattern, line):
                print(f"\n!! [{self.name}] FAULT: {line}", flush=True)
                self._pending_fault = Fault(
                    self.name, pattern, line, list(self._recent)
                )
                # Enough to carry a register dump and a backtrace.
                self._fault_tail = 28
                return


# The controller turns a transient Temporal call into a 502, so a long soak
# will meet one eventually. Losing a fourteen-round fault report to a single
# blip is worse than the blip.
TRANSIENT_STATUSES = frozenset({429, 500, 502, 503, 504})


def request_json(
    controller: str,
    path: str,
    *,
    method: str = "GET",
    payload: dict[str, JsonValue] | None = None,
    attempts: int = 4,
) -> dict[str, JsonValue]:
    """Call one controller JSON endpoint and validate its top-level shape.

    Retries transient failures. A 409 is not transient -- it is the controller
    saying a round is already running -- so it is raised on the first try.
    """
    body = json.dumps(payload).encode() if payload is not None else None
    last: Exception | None = None
    for attempt in range(1, attempts + 1):
        request = Request(
            f"{controller.rstrip('/')}{path}",
            data=body,
            method=method,
            headers={"Content-Type": "application/json"},
        )
        try:
            with urlopen(request, timeout=15) as response:
                decoded: object = json.loads(response.read())
        except HTTPError as error:
            last = error
            if error.code not in TRANSIENT_STATUSES:
                break
        except (URLError, TimeoutError) as error:
            last = error
        else:
            if not isinstance(decoded, dict):
                raise TypeError("controller returned a non-object JSON response")
            return cast(dict[str, JsonValue], decoded)
        if attempt < attempts:
            print(f"controller {path} failed ({last}); retrying", flush=True)
            time.sleep(2.0 * attempt)
    raise RuntimeError(f"controller request failed: {last}") from last


def listed_badges(controller: str) -> set[str]:
    """Callsigns the controller currently counts as polling Workers."""
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


def wait_for_scheduler_roster(
    controller: str, callsigns: list[str], timeout: float
) -> None:
    """Wait until the controller lists every callsign as a polling Worker.

    A badge reporting ``polling=true`` has only started its Worker task. The
    round is sized from Temporal's own poller list, which lags that by seconds,
    so starting on the firmware flag alone produces a round sized for fewer
    badges than are present -- or for none at all.
    """
    deadline = time.monotonic() + timeout
    wanted = set(callsigns)
    seen: set[str] = set()
    while time.monotonic() < deadline:
        seen = listed_badges(controller)
        if wanted <= seen:
            return
        time.sleep(0.5)
    raise TimeoutError(
        f"controller never listed every badge: wanted {sorted(wanted)}, saw {sorted(seen)}"
    )


def discover_ports(explicit: list[str], *, expected: int | None = None) -> list[str]:
    """Return the badge serial devices to drive."""
    ports = explicit or sorted(glob("/dev/cu.usbmodem*"))
    if len(ports) != len(set(ports)) or not ports:
        raise RuntimeError(f"expected distinct badge ports, found {ports}")
    if expected is not None and len(ports) != expected:
        raise RuntimeError(f"expected exactly {expected} badge ports, found {ports}")
    return ports

"""Bounded HTTP primitive for the optional evaluation-only TypeSafe reader.

One request, no retries. This is source support, not a historical run receipt.
The caller owns explicit live authorization, credentials and budget reservation.
"""
from __future__ import annotations

import http.client
import json
import signal
import threading
import time

HOST = "api.typesafe.ai"
ENDPOINT = "/v1/systemone"
MAX_RESPONSE_BYTES = 2_000_000
DEADLINE_SECONDS = 2


def check(ok: bool, message: str) -> None:
    if not ok:
        raise ValueError(message)


class DeadlineExceeded(TimeoutError):
    pass


def _deadline(_signum: int, _frame: object) -> None:
    raise DeadlineExceeded("request wall-clock deadline exceeded")


def call(conn: http.client.HTTPSConnection | None, body: bytes, key: str
         ) -> tuple[int | None, object | None, str | None, float, http.client.HTTPSConnection | None]:
    """One attempt, never retried; a failed connection is discarded."""
    check(threading.current_thread() is threading.main_thread(), "live runner requires main thread")
    check(not any(signal.getitimer(signal.ITIMER_REAL)), "existing real-time alarm")
    start = time.perf_counter()
    previous_handler = signal.getsignal(signal.SIGALRM)
    status = None
    try:
        signal.signal(signal.SIGALRM, _deadline)
        signal.setitimer(signal.ITIMER_REAL, DEADLINE_SECONDS)
        if conn is None:
            conn = http.client.HTTPSConnection(HOST, timeout=DEADLINE_SECONDS)
        conn.request("POST", ENDPOINT, body, {"Content-Type": "application/json", "Authorization": "Bearer " + key})
        reply = conn.getresponse()
        status = reply.status
        raw = reply.read(MAX_RESPONSE_BYTES + 1)
        check(len(raw) <= MAX_RESPONSE_BYTES, "response too large")
        check(key.encode() not in raw, "credential echo")
        if status != 200:
            conn.close()
            return status, None, f"HTTP {status}", (time.perf_counter() - start) * 1000, None
        result = json.loads(raw, parse_constant=lambda x: (_ for _ in ()).throw(ValueError("non-finite response")))
        return status, result, None, (time.perf_counter() - start) * 1000, conn
    except Exception as exc:
        if conn is not None:
            conn.close()
        return status, None, type(exc).__name__, (time.perf_counter() - start) * 1000, None
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)


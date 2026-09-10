"""Bounded wall-time counters; no per-frame history or GPU synchronization."""
from __future__ import annotations

import json
import os
import threading
import time
import urllib.error
import urllib.request
from contextlib import contextmanager
from functools import wraps

from .auth import authorize

STAGES = (
    "decode", "face", "hands", "pose", "segmentation", "observations_publish",
    "mask_publish", "avatar_tracking", "avatar_render", "avatar_publish",
    "depth", "depth_refine", "depth_publish",
)


class StageTelemetry:
    def __init__(self):
        self._lock = threading.Lock()
        self._started = time.perf_counter()
        self._values = {name: [0, 0.0, 0.0] for name in STAGES}

    @contextmanager
    def measure(self, name):
        started = time.perf_counter()
        try:
            yield
        finally:
            duration = (time.perf_counter() - started) * 1000
            with self._lock:
                value = self._values[name]
                value[0] += 1
                value[1] += duration
                value[2] = max(value[2], duration)

    def snapshot(self):
        with self._lock:
            now = time.perf_counter()
            interval = max(now - self._started, 0.000001)
            values = self._values
            self._values = {name: [0, 0.0, 0.0] for name in STAGES}
            self._started = now
        return {
            "pid": os.getpid(), "interval_ms": interval * 1000,
            "stages": {
                name: {
                    "calls": count, "total_ms": total,
                    "max_ms": maximum if count else None,
                } for name, (count, total, maximum) in values.items()
            },
        }

    @contextmanager
    def publishing(self, daemon_url):
        stop = threading.Event()
        self.snapshot()

        def publish():
            while not stop.wait(2):
                body = json.dumps(self.snapshot()).encode()
                request = urllib.request.Request(
                    f"{daemon_url.rstrip('/')}/api/v1/perception/telemetry",
                    data=body, headers={"Content-Type": "application/json"}, method="POST",
                )
                try:
                    with urllib.request.urlopen(authorize(request), timeout=1):  # noqa: S310
                        pass
                except (OSError, urllib.error.URLError):
                    pass  # Telemetry must never interrupt camera processing.

        thread = threading.Thread(target=publish, name="tarsier-metrics", daemon=True)
        thread.start()
        try:
            yield
        finally:
            stop.set()
            thread.join(timeout=2)


stages = StageTelemetry()


def timed(name):
    def decorate(function):
        @wraps(function)
        def wrapped(*args, **kwargs):
            with stages.measure(name):
                return function(*args, **kwargs)
        return wrapped
    return decorate

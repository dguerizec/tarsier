from __future__ import annotations

from collections.abc import Callable
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class PortraitVersion:
    revision: int
    source: Any


class PortraitSwitcher:
    """Prepare off the render thread and commit only a successfully published frame."""

    def __init__(
        self,
        revision: int,
        source: Any,
        prepare: Callable[[Path], Any],
        render: Callable[[Any, Any], Any],
        report_error: Callable[[int, str], None],
    ) -> None:
        self.active = PortraitVersion(revision, source)
        self._prepare = prepare
        self._render = render
        self._report_error = report_error
        self._executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="portrait-source")
        self._future: Future | None = None
        self._loading_revision: int | None = None
        self._requested_revision = revision
        self._requested_path: Path | None = None
        self._ready: PortraitVersion | None = None
        self._failed_revision: int | None = None

    def request(self, revision: int, path: Path) -> None:
        if revision == self._requested_revision:
            return
        self._requested_revision = revision
        self._requested_path = path
        self._ready = None
        self._failed_revision = None

    def prepare_pending(self) -> None:
        if self._future is not None and self._future.done():
            future, revision = self._future, self._loading_revision
            self._future = None
            try:
                source = future.result()
                if revision == self._requested_revision:
                    self._ready = PortraitVersion(revision, source)
            except Exception as error:
                if revision == self._requested_revision:
                    self._failed_revision = revision
                    self._executor.submit(self._report_error, revision, str(error))
        if (
            self._future is None
            and self._ready is None
            and self._requested_revision != self.active.revision
            and self._requested_revision != self._failed_revision
            and self._requested_path is not None
        ):
            self._loading_revision = self._requested_revision
            self._future = self._executor.submit(self._prepare, self._requested_path)

    def render(self, driving: Any) -> tuple[Any, PortraitVersion]:
        self.prepare_pending()
        candidate = self._ready or self.active
        try:
            return self._render(driving, candidate.source), candidate
        except Exception as error:
            if candidate is self.active:
                raise
            self._ready = None
            self._failed_revision = candidate.revision
            self._executor.submit(self._report_error, candidate.revision, str(error))
            return self._render(driving, self.active.source), self.active

    def accept(self, revision: int) -> None:
        if self._ready is not None and self._ready.revision == revision:
            self.active = self._ready
            self._ready = None

    def close(self) -> None:
        # Join before the shared neural weights are released by the enclosing ExitStack.
        if self._future is not None:
            self._future.cancel()
        self._executor.shutdown(wait=True)
        self._future = None
        self._ready = None

    def __enter__(self) -> PortraitSwitcher:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

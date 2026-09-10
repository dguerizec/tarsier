"""Read the daemon's aggregate model demand without coupling it to UI visibility."""
import json
import logging
import time
import urllib.error
import urllib.request

from .auth import authorize

MODELS = ("face", "hands", "pose")


class DetectionDemandClient:
    def __init__(self, daemon_url: str):
        self._url = daemon_url.rstrip("/") + "/api/v1/perception/demand"
        self._next = 0.0
        self._models = dict.fromkeys(MODELS, True)

    def models(self) -> dict[str, bool]:
        now = time.monotonic()
        if now >= self._next:
            self._next = now + 0.25
            try:
                with urllib.request.urlopen(  # noqa: S310
                    authorize(urllib.request.Request(self._url)), timeout=1
                ) as response:
                    value = json.load(response)
                if not isinstance(value, dict) or any(
                    type(value.get(k)) is not bool for k in MODELS
                ):
                    raise ValueError("invalid model demand")
                self._models = {k: value[k] for k in MODELS}
            except (OSError, ValueError, urllib.error.URLError):
                # Preserve gestures if the demand endpoint is unavailable, including
                # compatibility with an older daemon. Retry at the bounded cadence.
                self._models = dict.fromkeys(MODELS, True)
                logging.getLogger(__name__).debug("model demand unavailable; enabling detectors")
        return self._models.copy()

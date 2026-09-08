"""Subscribe to gesture-gated PCM and transcribe in memory with local Whisper."""

import argparse
import asyncio
import json
import os
import sys
import urllib.error
import urllib.request
from collections import deque
from contextlib import suppress
from dataclasses import dataclass, field
from urllib.parse import urlsplit, urlunsplit

import numpy as np
from scipy.signal import resample_poly
from websockets.asyncio.client import connect

RATE = 48_000
FRAME_BYTES = 4
MAX_AUDIO_BYTES = 125 * RATE * FRAME_BYTES


def whisper_audio(pcm: bytes) -> np.ndarray:
    """Convert interleaved stereo s16le to anti-aliased mono float32 at 16 kHz."""
    if len(pcm) % FRAME_BYTES:
        raise ValueError("PCM is not aligned to stereo sample frames")
    stereo = np.frombuffer(pcm, dtype="<i2").reshape(-1, 2).astype(np.float32)
    mono = stereo.mean(axis=1) / 32768.0
    return resample_poly(mono, 1, 3).astype(np.float32) if len(mono) else mono


@dataclass
class Utterance:
    id: str
    pcm: bytearray = field(default_factory=bytearray)
    reason: str = "condition"


class Transcriber:
    def __init__(self, decode, emit, interval=1.0):
        self.decode = decode
        self.emit = emit
        self.interval = interval
        self.current = None
        self.finals = deque()
        self.changed = asyncio.Event()
        self.stopped = False
        self.last_partial = None

    def receive(self, message):
        if isinstance(message, bytes):
            if self.current is None:
                raise ValueError("Audio arrived outside an utterance")
            if len(message) % FRAME_BYTES or len(self.current.pcm) + len(message) > MAX_AUDIO_BYTES:
                raise ValueError("Invalid or oversized audio")
            self.current.pcm.extend(message)
            return
        value = json.loads(message)
        kind = value.get("type")
        if kind == "subscribed":
            if value.get("format", {}).get("encoding") != "pcm_s16le" or (
                value["format"].get("sample_rate"),
                value["format"].get("channels"),
            ) != (RATE, 2):
                raise ValueError("Unsupported audio format")
            self.emit(value)
        elif kind == "started":
            if self.current is not None:
                raise ValueError("Overlapping utterances")
            self.current = Utterance(value["utterance_id"])
            self.emit(value)
        elif kind == "ended":
            if self.current is None or self.current.id != value["utterance_id"]:
                raise ValueError("Unmatched utterance end")
            if len(self.finals) >= 2:
                raise ValueError("Whisper cannot keep up with completed utterances")
            self.current.reason = value["reason"]
            self.finals.append(self.current)
            self.current = None
            self.emit(value)
            self.changed.set()
        elif kind == "error":
            raise ValueError(f"Subscription rejected: {value.get('code')}")
        elif kind == "closed":
            self.emit(value)
            return False
        return True

    def stop(self):
        if self.current is not None:
            self.current.reason = "disconnected"
            self.finals.append(self.current)
            self.current = None
        self.stopped = True
        self.changed.set()

    async def run(self):
        while not self.stopped or self.finals:
            if not self.finals:
                with suppress(TimeoutError):
                    await asyncio.wait_for(self.changed.wait(), self.interval)
                self.changed.clear()
            final = bool(self.finals)
            utterance = self.finals.popleft() if final else self.current
            if utterance is None:
                continue
            snapshot = bytes(utterance.pcm)
            revision = (utterance.id, len(snapshot))
            if not final and (
                len(snapshot) < RATE * FRAME_BYTES // 2 or revision == self.last_partial
            ):
                continue
            self.last_partial = revision
            # Recognition never blocks the task receiving PCM or end events.
            text = await asyncio.to_thread(self.decode, snapshot) if snapshot else ""
            # Suppress obsolete partial results if the end arrived during inference.
            if not final and self.current is not utterance:
                continue
            self.emit(
                {
                    "type": "final" if final else "partial",
                    "utterance_id": utterance.id,
                    "text": text,
                    "audio_ms": len(snapshot) * 1000 / (RATE * FRAME_BYTES),
                    "reason": utterance.reason if final else None,
                }
            )


def output(value, as_json):
    if as_json:
        print(json.dumps(value, ensure_ascii=False), flush=True)
        return
    kind = value["type"]
    if kind == "subscribed":
        print(
            "Subscribed. Make the phone gesture and speak; lower your hand to finish.", flush=True
        )
    elif kind == "started":
        print(f"\nStarted — pre-roll {value['pre_roll_ms']:.0f} ms", flush=True)
    elif kind == "ended":
        print(f"Ended — {value['audio_ms']:.0f} ms audio — {value['reason']}", flush=True)
    elif kind in {"partial", "final"}:
        reason = value.get("reason")
        suffix = f" ({reason})" if reason and reason != "condition" else ""
        print(f"{kind.upper()}{suffix}: {value['text'] or '(no speech recognized)'}", flush=True)
    elif kind == "closed":
        print(f"Subscription closed: {value['reason']}", flush=True)


def connection(args):
    parsed = urlsplit(args.url)
    if parsed.scheme not in {"http", "https"} or parsed.username or parsed.password:
        raise ValueError("Use an http(s) base URL without embedded credentials")
    if parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
        raise ValueError("Use a server base URL without path, query or fragment")
    base = args.url.rstrip("/")
    token = os.environ.get("TARSIER_API_TOKEN", "").strip()
    if not token:
        raise ValueError("Set TARSIER_API_TOKEN to a token with the API destination enabled")
    if any(character.isspace() for character in token):
        raise ValueError("TARSIER_API_TOKEN must not contain whitespace")
    headers = {"Authorization": f"Bearer {token}"}
    source = args.source
    if source is None:
        with urllib.request.urlopen(
            urllib.request.Request(base + "/api/v1/audio/virtual", headers=headers), timeout=10
        ) as response:
            source = json.load(response).get("source")
        if not source:
            raise ValueError("Select an audio input in Tarsier or pass --source")
    scheme = "wss" if parsed.scheme == "https" else "ws"
    url = urlunsplit((scheme, parsed.netloc, "/api/v1/audio/utterances", "", ""))
    return url, headers, source


async def consume(url, headers, subscription, transcriber):
    async with connect(
        url,
        additional_headers=headers,
        max_size=1024 * 1024,
        max_queue=16,
        ping_interval=10,
        ping_timeout=5,
    ) as socket:
        await socket.send(json.dumps(subscription))
        task = asyncio.create_task(transcriber.run())
        try:
            while True:
                received = asyncio.create_task(socket.recv())
                done, _ = await asyncio.wait(
                    {received, task}, timeout=5, return_when=asyncio.FIRST_COMPLETED
                )
                if task in done:
                    received.cancel()
                    await task
                    raise RuntimeError("Transcriber stopped unexpectedly")
                if received not in done:
                    received.cancel()
                    raise TimeoutError("Tarsier stopped sending audio or heartbeats")
                if transcriber.receive(received.result()) is False:
                    break
        finally:
            transcriber.stop()
            await task


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8742")
    parser.add_argument(
        "--source", help="Enabled capture source ID; defaults to Tarsier's selected input"
    )
    parser.add_argument("--start-event", default="gesture.phone_near_mouth.started")
    parser.add_argument("--end-event", default="gesture.phone_near_mouth.ended")
    parser.add_argument("--pre-roll-ms", type=int, default=300)
    parser.add_argument("--max-duration-ms", type=int, default=60000)
    parser.add_argument(
        "--model", default="base", help="Whisper model name or local model directory"
    )
    parser.add_argument("--language", default="fr")
    parser.add_argument("--device", choices=["cpu", "cuda"], default="cpu")
    parser.add_argument(
        "--interval", type=float, default=1.0, help="Partial transcription interval"
    )
    parser.add_argument(
        "--json", action="store_true", help="Emit JSON lines instead of readable text"
    )
    args = parser.parse_args()
    if not 0 <= args.pre_roll_ms <= 5000 or not 100 <= args.max_duration_ms <= 120000:
        parser.error("pre-roll must be 0..5000 ms; max duration must be 100..120000 ms")
    if not 0.25 <= args.interval <= 10:
        parser.error("interval must be 0.25..10 seconds")
    try:
        url, headers, source = connection(args)
        from faster_whisper import WhisperModel

        print(f"Loading local Whisper {args.model} ({args.device})…", file=sys.stderr, flush=True)
        model = WhisperModel(
            args.model,
            device=args.device,
            compute_type="int8" if args.device == "cpu" else "float16",
            cpu_threads=4,
        )

        def decode(pcm):
            segments, _ = model.transcribe(
                whisper_audio(pcm),
                language=args.language,
                beam_size=1,
                condition_on_previous_text=False,
                vad_filter=False,
            )
            return " ".join(segment.text.strip() for segment in segments).strip()

        subscription = {
            "type": "subscribe",
            "source": source,
            "start": {"event": args.start_event},
            "end": {"event": args.end_event},
            "pre_roll_ms": args.pre_roll_ms,
            "max_duration_ms": args.max_duration_ms,
        }
        transcriber = Transcriber(decode, lambda value: output(value, args.json), args.interval)
        asyncio.run(consume(url, headers, subscription, transcriber))
    except KeyboardInterrupt:
        pass
    except Exception as error:
        print(f"Transcription stopped: {error}", file=sys.stderr)
        sys.exit(1)

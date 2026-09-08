"""Opt-in local TTS -> timed WebSocket PCM -> real Whisper test, without audio files."""

import asyncio
import json
import os
import subprocess
from pathlib import Path

import numpy as np
import pytest
from scipy.signal import resample_poly
from websockets.asyncio.server import serve

from tarsier_whisper import Transcriber, consume, whisper_audio


@pytest.mark.skipif(
    not os.environ.get("TARSIER_TEST_PIPER_MODEL"), reason="requires local Piper model"
)
def test_live_whisper_preserves_the_first_and_last_words():
    from faster_whisper import WhisperModel

    path = Path(os.environ["TARSIER_TEST_PIPER_MODEL"])
    rate = json.loads(path.with_suffix(path.suffix + ".json").read_text())["audio"]["sample_rate"]
    speech = subprocess.run(
        ["piper", "--model", str(path), "--output-raw"],
        input=(
            b"Bonjour, ceci est un test de transmission. "
            b"Je parle pendant le geste et je termine ma phrase.\n"
        ),
        capture_output=True,
        check=True,
    ).stdout
    mono = np.frombuffer(speech, dtype="<i2").astype(np.float32)
    resampled = np.clip(resample_poly(mono, 48000, rate), -32768, 32767).astype("<i2")
    pcm = np.repeat(resampled[:, None], 2, axis=1).tobytes()
    pcm += bytes((-len(pcm)) % 3840)
    model = WhisperModel("base", device="cpu", compute_type="int8", cpu_threads=4)

    def decode(data):
        segments, _ = model.transcribe(
            whisper_audio(data),
            language="fr",
            beam_size=1,
            condition_on_previous_text=False,
            vad_filter=False,
        )
        return " ".join(segment.text.strip() for segment in segments)

    async def scenario():
        output = []

        async def handler(socket):
            request = json.loads(await socket.recv())
            assert request["type"] == "subscribe"
            await socket.send(
                json.dumps(
                    {
                        "type": "subscribed",
                        "format": {
                            "encoding": "pcm_s16le",
                            "sample_rate": 48000,
                            "channels": 2,
                        },
                    }
                )
            )
            await socket.send(
                json.dumps({"type": "started", "utterance_id": "speech", "pre_roll_ms": 0})
            )
            for start in range(0, len(pcm), 3840):
                await socket.send(pcm[start : start + 3840])
                await asyncio.sleep(0.02)
            await socket.send(
                json.dumps(
                    {
                        "type": "ended",
                        "utterance_id": "speech",
                        "reason": "condition",
                        "audio_ms": len(pcm) / 192,
                    }
                )
            )
            await socket.send(json.dumps({"type": "closed", "reason": "unsubscribed"}))

        async with serve(handler, "127.0.0.1", 0) as server:
            port = server.sockets[0].getsockname()[1]
            await consume(
                f"ws://127.0.0.1:{port}",
                {},
                {"type": "subscribe"},
                Transcriber(decode, output.append),
            )
        finals = [value for value in output if value["type"] == "final"]
        partials = [value for value in output if value["type"] == "partial"]
        print(
            json.dumps({"partial_results": len(partials), "final": finals[-1]}, ensure_ascii=False)
        )
        assert partials
        text = finals[-1]["text"].lower()
        assert "bonjour" in text and "phrase" in text
        assert finals[-1]["audio_ms"] == len(pcm) / 192

    asyncio.run(scenario())

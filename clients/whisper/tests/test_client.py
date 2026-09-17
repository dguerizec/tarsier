import asyncio
import json
import threading
from types import SimpleNamespace

import numpy as np
import pytest
from websockets.asyncio.server import serve

from tarsier_whisper import MAX_AUDIO_BYTES, Transcriber, connection, consume, whisper_audio


def message(kind, **data):
    return json.dumps({"type": kind, **data})


def test_connection_requires_a_token_before_any_network_request(monkeypatch):
    monkeypatch.delenv("TARSIER_API_TOKEN", raising=False)
    with pytest.raises(ValueError, match="Set TARSIER_API_TOKEN"):
        connection(SimpleNamespace(url="http://127.0.0.1:8742", source=None))


def test_connection_uses_bearer_auth_without_resolving_physical_input(monkeypatch):
    monkeypatch.setenv("TARSIER_API_TOKEN", "test-api-token")
    url, headers = connection(SimpleNamespace(url="http://127.0.0.1:8742"))
    assert url == "ws://127.0.0.1:8742/api/v1/audio/utterances"
    assert headers == {"Authorization": "Bearer test-api-token"}


def test_invalid_token_is_rejected_without_disclosing_it(monkeypatch):
    monkeypatch.setenv("TARSIER_API_TOKEN", "secret\ninvalid")
    with pytest.raises(ValueError, match="must not contain whitespace") as error:
        connection(SimpleNamespace(url="http://127.0.0.1:8742", source="test-mic"))
    assert "secret" not in str(error.value)


def test_pcm_conversion_preserves_duration_and_stereo_average():
    stereo = np.tile(np.array([16384, 0], dtype="<i2"), (48000, 1))
    audio = whisper_audio(stereo.tobytes())
    assert audio.shape == (16000,)
    assert audio.dtype == np.float32
    assert audio[100:-100].mean() == pytest.approx(0.25, abs=0.001)
    with pytest.raises(ValueError, match="aligned"):
        whisper_audio(b"x")


def test_receiving_audio_does_not_wait_for_whisper_and_final_has_the_complete_utterance():
    async def scenario():
        entered = threading.Event()
        release = threading.Event()
        decoded = []
        output = []

        def decode(pcm):
            decoded.append(len(pcm))
            if len(decoded) == 1:
                entered.set()
                assert release.wait(2)
            return f"{len(pcm)} bytes"

        client = Transcriber(decode, output.append, interval=0.01)
        task = asyncio.create_task(client.run())
        client.receive(message("started", utterance_id="one", pre_roll_ms=300))
        client.receive(bytes(96000))
        assert await asyncio.to_thread(entered.wait, 2)
        client.receive(bytes(96000))
        client.receive(message("ended", utterance_id="one", reason="condition", audio_ms=1000))
        client.stop()
        release.set()
        await task
        assert decoded == [96000, 192000]
        finals = [value for value in output if value["type"] == "final"]
        assert finals[0]["audio_ms"] == 1000
        assert finals[0]["text"] == "192000 bytes"
        assert not [value for value in output if value["type"] == "partial"]

    asyncio.run(scenario())


def test_protocol_rejects_orphan_audio_mismatched_end_and_unbounded_buffers():
    client = Transcriber(lambda _: "", lambda _: None)
    with pytest.raises(ValueError, match="outside"):
        client.receive(bytes(4))
    client.receive(message("started", utterance_id="one", pre_roll_ms=0))
    with pytest.raises(ValueError, match="Unmatched"):
        client.receive(message("ended", utterance_id="two", reason="condition"))
    with pytest.raises(ValueError, match="oversized"):
        client.receive(bytes(MAX_AUDIO_BYTES + 4))
    client.stop()
    assert client.finals[0].reason == "disconnected"


def test_websocket_subscription_receives_live_audio_without_creating_a_file(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)

    async def scenario():
        output = []

        async def handler(socket):
            request = json.loads(await socket.recv())
            assert request["pre_roll_ms"] == 0
            await socket.send(
                message(
                    "subscribed",
                    format={
                        "encoding": "pcm_s16le",
                        "sample_rate": 48000,
                        "channels": 2,
                    },
                )
            )
            await socket.send(message("started", utterance_id="test", pre_roll_ms=0))
            await socket.send(bytes(3840))
            await socket.send(
                message("ended", utterance_id="test", reason="condition", audio_ms=20)
            )
            await socket.send(message("closed", reason="unsubscribed"))

        async with serve(handler, "127.0.0.1", 0) as server:
            port = server.sockets[0].getsockname()[1]
            client = Transcriber(lambda pcm: f"{len(pcm)} bytes", output.append)
            await consume(
                f"ws://127.0.0.1:{port}", {}, {"type": "subscribe", "pre_roll_ms": 0}, client
            )
        finals = [value for value in output if value["type"] == "final"]
        assert len(finals) == 1
        assert finals[0]["text"] == "3840 bytes"

    asyncio.run(scenario())
    assert list(tmp_path.iterdir()) == []

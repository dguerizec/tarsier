# Event-gated audio subscriptions

External clients connect to `ws://127.0.0.1:8742/api/v1/audio/utterances` (or
`wss://` behind HTTPS). This endpoint requires the existing API token destination
when authentication is enabled. Send the token in the `Authorization: Bearer …`
header, never in the URL. Browser sessions are also supported. Authentication is
checked at upgrade, subscription acceptance, every opening event and once a
second during the connection. Revoking the token terminates its live subscription.

The first WebSocket message must arrive within 10 seconds:

```json
{
  "type": "subscribe",
  "device": "tarsier_satellites",
  "start": {"event": "gesture.phone_near_mouth.started"},
  "end": {"event": "gesture.phone_near_mouth.ended"},
  "pre_roll_ms": 300,
  "max_duration_ms": 60000
}
```

Select and enable an input in Tarsier's **Satellite microphone** section, then
turn that device on. The stable virtual device ID is `tarsier_satellites`.
Satellite clients pass only this device ID; physical source IDs and the old
`source` subscription field are rejected. No input enumeration is needed.

This is a daemon-owned API audio device, delivered over WebSocket; it does not
create a separate OS/PipeWire recording device. Its input route, enable state and
mute are persisted by Tarsier through `GET/POST /api/v1/audio/satellite`:
`{"enabled":true,"source":"physical-input-id","muted":false}`. Only the Tarsier
configuration interface needs the physical ID. Selecting an input for an enabled
device enables capture of that input; a subscription itself never enables capture.

The device has its own route and mute, independent of Tarsier Microphone's route,
mute, AGC, noise reduction and voice conversion. Switching its input keeps both
the WebSocket and any active utterance open. Muted, missing, disabled or unplugged
inputs produce silence at the same 48 kHz stereo rate, with no automatic fallback.
Switching can insert a short silence while the new capture becomes ready; it is
not a sample-perfect crossfade. Pre-roll belongs to the virtual device and can
contain audio from its previous route. Turning the satellite device off ends its
subscriptions with `device_disabled`; Allô retries until it is enabled again.

Conditions match semantic event `kind` exactly; they are data, not executable
expressions or commands. The client only receives audio boundaries and PCM for
its subscription, not the daemon's full event/state broadcast. An opening event
already in progress at subscription time is not replayed. Repeated starts while
an utterance is active (including its final audio block) are ignored; end events
while idle are ignored. A fresh opening event is required after completion or a
maximum-duration cutoff.

Each connection owns one subscription. Closing it or sending
`{"type":"unsubscribe"}` destroys the subscription and its in-memory buffer.
Reconnect to change conditions. Subscriptions are independent and do not persist
across reconnects or daemon restarts. There is currently no exclusivity or priority
between subscribers and the virtual microphone. The validation/authorization
boundary is centralized for future source/event permissions; current tokens still
use the existing API-wide destination permission.

## Audio and messages

The server acknowledges the accepted settings:

```json
{
  "type": "subscribed",
  "subscription_id": "connection-specific-id",
  "subscription": {"type":"subscribe", "device":"tarsier_satellites", "start":{"event":"gesture.phone_near_mouth.started"}, "end":{"event":"gesture.phone_near_mouth.ended"}, "pre_roll_ms":300, "max_duration_ms":60000},
  "format": {"encoding":"pcm_s16le", "sample_rate":48000, "channels":2, "block_ms":20},
  "processing": "routed_capture"
}
```

For each utterance, WebSocket message ordering is:

1. Text JSON `started`: `utterance_id`, opening `event_sequence`, and actual
   available `pre_roll_ms` (which can be less than requested after subscribing).
2. Binary PCM blocks: signed 16-bit little-endian, stereo interleaved, 48 kHz.
   Boundary blocks may be shorter than 20 ms. No WAV header or file is produced.
3. Text JSON `ended`: same `utterance_id`, `reason`, closing `event_sequence`
   when applicable, total stereo sample-frame count `samples`, and `audio_ms`
   including pre-roll. The normal closing reason is `condition`.

The routed capture is independent of Tarsier Microphone's processing and mute.
Use the dedicated Satellite microphone mute to silence this device. Conference
and agent priority controls are not implemented here.

`pre_roll_ms` defaults to 300 and accepts 0–5000. Zero excludes audio before the
opening event. A positive value retains only a bounded RAM history, beginning at
subscription acceptance, and transmits it only on opening. The start/end cutoffs
use daemon virtual-device timestamps and sample-aligned clipping; they are not
hardware-clock measurements of the physical microphone. On an end event the server
waits briefly for the next capture block to include the remaining tail, clips at
the closing timestamp and then sends `ended`.

`max_duration_ms` defaults to 60000 and accepts 100–120000, excluding pre-roll.
It prevents a missing or misspelled closing condition from leaving an utterance
open indefinitely. The server admits at most 16 utterance connections and limits
incoming control messages to 4096 bytes. Each send batch has a 250 ms timeout;
clients must receive continuously, independently of transcription or other work.

A `heartbeat` text message is sent once per second and carries the current
utterance ID in `active`, or `null`. An invalid request receives `error` with a
`code` and the connection closes. Shutdown, disabled device, revocation, stalled
audio, lost audio/events or an unsubscribe produce a best-effort `ended` and
`closed` with a reason, then close the connection. `audio_tail_timeout` indicates
that a final capture block did not arrive; `audio_lost`/`events_lost` indicate a
subscriber fell behind. These are interruptions, not successful end conditions.
Clients must also end their local action on disconnect/heartbeat timeout: a
process crash or broken network cannot reliably deliver a final message.

## Local live Whisper client

From the repository root:

```sh
read -rsp 'Tarsier API token: ' TARSIER_API_TOKEN
echo
export TARSIER_API_TOKEN
uv run --project clients/whisper tarsier-whisper
```

Sign in as a human through Tarsier's web interface and create a token with the
**API** destination enabled. Paste that token into the hidden terminal prompt
above; the token value is not part of shell command history. Machine clients use
tokens, never the human password or browser session. The Whisper client requires
`TARSIER_API_TOKEN`, fails before connecting if it is absent, and has no password
login option. For a service, supply this variable through its environment.
The client requests `tarsier_satellites` by default. `--audio-device` identifies
a Tarsier virtual device; physical inputs are configured only in Tarsier.

The client loads local [faster-whisper](https://github.com/SYSTRAN/faster-whisper)
with the multilingual `base` model, French recognition and CPU int8 by default.
The first model load downloads weights to the normal model cache; audio is never
sent to a model service. Model weights are cached, but this client writes neither
audio files nor transcript files. Text is printed to stdout; redirecting stdout
or running under a logging service can persist that text.

Make the phone gesture and speak. The terminal shows the actual pre-roll, partial
text while speaking, the audio duration and a final transcription after release.
Whisper performs repeated decoding of the in-memory utterance: partial text can
change, and its latency depends on model/hardware. Inference runs separately from
PCM reception. Completed utterances have a bounded queue; overload fails visibly
rather than silently dropping audio. No recognition is done on the pre-roll while
the subscription is idle.

Useful options:

```sh
# Longer pre-roll; zero gives strict event boundaries.
uv run --project clients/whisper tarsier-whisper --pre-roll-ms 500

# Machine-readable partial/final text on stdout.
uv run --project clients/whisper tarsier-whisper --json

# Tune model/language or partial-result interval.
uv run --project clients/whisper tarsier-whisper --model base --language fr --interval 1
```

`--device cuda` is optional and requires a compatible CTranslate2 CUDA/cuDNN
installation. CPU execution is the tested default. `--start-event`, `--end-event`
and `--max-duration-ms` customize the subscription from the client.

## Validation

```sh
cargo test --bin tarsier utterances
uv run --project clients/whisper pytest -q clients/whisper/tests
uv run --project clients/whisper ruff check clients/whisper
```

The opt-in real Whisper test synthesizes French speech through a locally installed
Piper voice and streams it in 20 ms blocks over a local test WebSocket. It checks
partial results, the first/last words and complete duration, entirely in memory:

```sh
TARSIER_TEST_PIPER_MODEL=/path/to/fr_FR-voice.onnx \
  uv run --project clients/whisper pytest -q -s clients/whisper/tests/test_live_whisper.py
```

This establishes network-client/recognition behavior on synthetic speech. Testing
the physical gesture and microphone together remains a separate live validation.

An additional opt-in test runs the real Rust subscription endpoint, delays the
opening event until speech has already started, then sends its gated PCM to the
actual Whisper CLI. It verifies that pre-roll preserves the first word:

```sh
TARSIER_TEST_PIPER_MODEL=/path/to/fr_FR-voice.onnx \
  cargo test --bin tarsier utterances_real_whisper -- --ignored --nocapture
```

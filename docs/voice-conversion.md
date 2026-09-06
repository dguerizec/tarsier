# Voice conversion experiment

Branch: `feat/voice-conversion`. UI/API: `http://127.0.0.1:8743`.

## Run the isolated baseline

From this worktree:

```sh
tools/voice-conversion-dev build
tools/voice-conversion-dev check
tools/voice-conversion-dev serve
```

The launcher pins build artifacts to this worktree's `target/` and persistent
state to `.voice-conversion/`. It sets the supported `TARSIER_USER_SETTINGS_PATH`,
`TARSIER_AUTH_PATH`, `TARSIER_PHOTOS_DIR`, and `TARSIER_VIDEOS_DIR` overrides.
It validates the isolation profile and checks the port before starting.
Do not launch this worktree with the default configuration.

The baseline uses synthetic 720p video, a mock camera, no V4L2 output, and no
perception/avatar worker. Only `tarsier_microphone` is eligible for shared input
capture. Enable its input meter in the audio UI. No physical input is opened or
reserved. Application termination and exclusive-capture requests are blocked.
Main's service, settings, devices, and default microphone must remain unchanged.

The experimental output name is `tarsier_voice_conversion`. Its creation is
currently **disabled at the daemon policy level**, including API requests.
The existing main daemon reserves every new source except its own fixed output;
merely using another name would make main attempt to capture the experiment's
output. Before publishing, use a separate PipeWire server with an explicit PCM
bridge, or coordinate a separately authorized main change to exclude virtual
sources. Do not enable output on the shared graph with the current main binary.

## RVC feasibility boundary

No target model or RVC runtime is installed by this baseline. It does not perform
voice conversion and provides no conversion latency result. A pretrained voice
model can be used; training a voice is not required for the first experiment.
The RVC inference model (`.pth`, optional retrieval `.index`) is distinct from the
shared content encoder and pitch model weights.

Primary sources inspected on 2026-09-07:

- https://github.com/RVC-Project/Retrieval-based-Voice-Conversion-WebUI
- https://github.com/w-okada/voice-changer

RVC exposes a real-time GUI, but its advertised latency is hardware-dependent.
Do not reuse upstream headline latency as a measurement of Tarsier. Pin an
upstream revision and dependencies in a separate environment after selecting a
compatible pretrained model. Keep that environment separate from `worker/.venv`.

The local GPU snapshot during preparation was an RTX 3070 with 7596/8192 MiB
allocated while main was running. This is a transient observation, but leaves
little space for another model. Measure actual peak use before enabling GPU RVC;
do not stop main or its workers to make room.

## Next bounded prototype

Use a separate worker with bounded PCM queues after input gain processing and
before final mute/output. Existing blocks are 20 ms, stereo S16LE at 48 kHz.
RVC needs explicit resampling/channel conversion and overlapping inference
windows. The 120 ms freshness cutoff of the current audio path must be reviewed
against measured conversion latency; do not silently replay stale output.

Mute must gate the final output after inference. Flush pending results on source,
model, or mode changes. Worker failure or overload must produce silence without
recreating the published virtual microphone. Track capture-to-output latency,
inference p50/p95, queue age, dropped blocks, CPU, and peak GPU memory. Measure
with a known signal and compare a bypass run, then repeat while video/avatars
are active. Audiovisual synchronization is outside this first trial.

## Validated baseline

The first baseline was launched as a separate transient user service:

```sh
systemd-run --user --unit=tarsier-voice-conversion --collect \
  --property=Restart=on-failure --property=RestartSec=3 \
  --property=KillMode=control-group --property=WorkingDirectory="$PWD" \
  "$PWD/tools/voice-conversion-dev" serve
```

Use `systemctl --user status tarsier-voice-conversion.service` to inspect it and
`systemctl --user stop tarsier-voice-conversion.service` to stop only this instance.
The transient unit is not installed for boot startup.

Validation: 183 Rust tests passed (one ignored), nine JavaScript tests passed,
profile validation passed, and the live health/preview endpoints responded.
Live API requests rejected virtual output, exclusive capture, and an input outside
the allowlist. The experimental process had no V4L2 descriptors; its only PCM
capture child used `--device tarsier_microphone` without `node.exclusive`.
Main retained its PID and default source. Its settings hash changed concurrently,
so unchanged settings cannot be claimed; the experimental process environment
was verified to point all four state/media paths into this worktree.

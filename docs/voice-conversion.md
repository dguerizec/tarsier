# Voice conversion experiment

Branch: `feat/voice-conversion`. UI/API: `http://127.0.0.1:8743`.

## Run the development server

The physical-device profile requires main's `tarsier.service` to be stopped.
The launcher checks this before opening devices. Never run both profiles with
physical camera capture enabled. Stop or start main only with user authorization.

From this worktree:

```sh
tools/voice-conversion-dev build
tools/voice-conversion-dev check
tools/voice-conversion-dev serve
```

The launcher pins build artifacts to this worktree's `target/` and persistent
state to `.voice-conversion/`. It sets `TARSIER_USER_SETTINGS_PATH`,
`TARSIER_AUTH_PATH`, `TARSIER_PHOTOS_DIR`, and `TARSIER_VIDEOS_DIR`.
Do not launch this worktree with the default configuration.

For a supervised instance that supports device changes from Settings:

```sh
systemd-run --user --unit=tarsier-voice-conversion --collect \
  --property=Restart=on-failure --property=RestartSec=3 \
  --property=KillMode=control-group --property=WorkingDirectory="$PWD" \
  "$PWD/tools/voice-conversion-dev" serve
```

Use `systemctl --user status tarsier-voice-conversion.service` to inspect it and
`systemctl --user stop tarsier-voice-conversion.service` to stop only this instance.
The transient unit is not installed for boot startup.

## Device selection

Open `/settings` and use **Camera and microphones**. Select a camera, check the
microphones to capture, and choose which selected microphone feeds audio output.
**Save and restart** persists these choices together and restarts only this
instance. Stop an active recording before changing devices. Refreshing the list
is explicit so hotplug discovery does not discard unsaved choices.

Camera choices prefer `/dev/v4l/by-id/` identities, then `/dev/v4l/by-path/`, with
`/dev/videoN` only when neither stable link exists. Physical primary interfaces
are listed; UVC metadata and loopback nodes are excluded. The OBSBOT Tiny 2 gets
its supported motor controls on the same stable path; other cameras use capture
without OBSBOT-specific commands. Synthetic video remains an option.

Microphones use PulseAudio/PipeWire source names. Saving device settings disables
automatic capture of unselected inputs, even when the base daemon policy normally
reserves all inputs. This development profile uses shared capture, never changes
the system default microphone, and names its output `tarsier_voice_conversion`.
The settings page selects inputs; the preview's existing output and mute controls
still determine whether the virtual microphone publishes audio.

The initial profile used synthetic video and shared capture from main's virtual
microphone, with experimental output blocked. After the user requested stopping
main, this profile was extended to physical inputs. V4L2 loopback output remains disabled. Perception, background segmentation,
depth, and all three avatar engines are enabled. The current main binary attempts to
reserve newly created sources; keep main stopped while this experiment runs.

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

An earlier GPU snapshot while main was running showed an RTX 3070 with
7596/8192 MiB allocated. Re-measure GPU use now that main has stopped before
sizing the RVC experiment.

## Next bounded prototype

Use a separate worker with bounded PCM queues after input gain processing and
before final mute/output. Existing blocks are 20 ms, stereo S16LE at 48 kHz.
RVC needs explicit resampling/channel conversion and overlapping inference
windows. Review the existing 120 ms freshness cutoff against measured conversion
latency; do not silently replay stale output.

Mute must gate the final output after inference. Flush pending results on source,
model, or mode changes. Worker failure or overload must produce silence without
recreating the published virtual microphone. Track capture-to-output latency,
inference p50/p95, queue age, dropped blocks, CPU, and peak GPU memory. Measure
with a known signal and compare a bypass run, then repeat while video/avatars
are active. Audiovisual synchronization is outside this first trial.

## Device selection validation

185 Rust tests passed (one ignored), including stable camera identity across
simulated renumbering and atomic device preference persistence. Nine JavaScript
tests passed and the settings script passed the syntax check.

The live API rejected an arbitrary camera path and an output microphone absent
from the capture selection. Switching to synthetic video with no microphones
released the capture children; restoring the OBSBOT by-id path and both USB
microphones survived the next supervised restart. Physical preview ran at
approximately 30 FPS with no pipeline error. Audio output stayed disabled and
main stayed inactive. Browser visual verification was unavailable in this session.


## Perception and avatar setup

The experimental worker has its own `worker/.venv`, installed with:

```sh
uv sync --project worker --locked --extra avatar --extra liveportrait --extra depth --link-mode hardlink
```

Shared pretrained model files in `~/.cache/tarsier/models` are verified by checksum
and read in place. The personalized LivePortrait image and Personal 3D export
are separate copies under `.voice-conversion/assets/liveportrait-source.png` and
`.voice-conversion/assets/portrait/`. They are local, ignored data, not committed
assets. Provision those files when preparing another checkout. The launcher
checks their presence and isolates TorchInductor/ Triton caches under the
experimental state directory. Compilation is disabled for predictable first use.

After enabling the worker, live checks confirmed fresh depth frames and avatar
frames for Stylized 3D, Personal 3D, and LivePortrait. LivePortrait continued
publishing with a latest-frame age of 69 ms at the final check; this is frame
freshness, not an end-to-end latency benchmark. The preview was restored to
Camera after validation. Main remained inactive. RVC voice conversion remains
separate, unfinished work.

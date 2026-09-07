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
main, this profile was extended to physical inputs. At the user's request, V4L2
loopback output is now enabled on the existing
`/dev/video42` (Tarsier Camera), allowing recordings and camera use in other apps.
Main must remain stopped while this worktree uses that shared virtual device.
Perception, background segmentation,
depth, and all three avatar engines are enabled. The current main binary attempts to
reserve newly created sources; keep main stopped while this experiment runs.

## RVC prototype

RVC is now integrated between automatic gain and final output mute. Install it
with `tools/voice-setup`, then use the Voice conversion control in Audio → Output.
The default demo voice is French Woman by DantSu, replacing the initial Japanese
Shigure Tokina trial after reported accent and background artifacts. Speech
quality still needs comparative listening tests. See [the voice bridge documentation](../voice/README.md)
for attribution, pinned assets, protocol, measured inference time, and limits.

The voice worker has a separate environment from perception. It opens no audio
device and receives only the selected input's PCM from the daemon. Conversion
failure produces silence while the virtual microphone remains published. Final
mute is enforced after conversion. A worker timeout never falls back to the raw
voice when conversion is enabled.

UI timing is daemon capture-to-publication delay, excluding audio hardware and
application buffering. Total microphone-to-ear latency and audiovisual
synchronization remain unmeasured. Video loopback is enabled on `/dev/video42`;
the visual processing worker,
preview, virtual camera and recordings can run during voice trials.

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
frames for Personal 3D and LivePortrait. LivePortrait continued
publishing with a latest-frame age of 69 ms at the final check; this is frame
freshness, not an end-to-end latency benchmark. The preview was restored to
Camera after validation. Main remained inactive. RVC voice conversion is enabled separately with its audio control.

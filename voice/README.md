# Experimental RVC voice bridge

Install from the worktree root with `tools/voice-setup`. This creates a separate
`voice/.venv` and downloads the assets listed with immutable revisions and SHA-256
checksums in `assets.json`. No perception-worker environment is modified.

The engine is [RVC WebUI](https://github.com/RVC-Project/Retrieval-based-Voice-Conversion-WebUI),
pinned to `81eed5e8f68b6bed1789f682fe78cdd324495afc` (MIT license). `worker.py`
adapts its real-time inference interface and overlap alignment, without the
upstream GUI or device capture. The runtime uses Torch/Torchaudio 2.11 with the
locked minimal dependencies; this differs from upstream's full GUI environment
and is verified here as an experimental integration.

## Demo voice and attribution

The demo model is **Shigure Tokina / 刻鳴時雨 (CV: Marukoro / 丸ころ)**,
managed by **Bindume / 瓶詰め**, trained and distributed by **yasyune**:

- Original model and terms: https://huggingface.co/yasyune/Shigure_Tokina_RVC
- Voice corpus and guidelines: https://bindume-chan.booth.pm/items/3640133

The model is downloaded from its original distributor, not the VCClient-only
sample mirror. It is not redistributed in this repository. Follow the linked
voice/model conditions when using or distributing results. French speech can be
processed, but this Japanese-trained voice is only a technical demonstration;
French pronunciation and accent quality are not validated.

## Audio behavior

The worker receives 160 ms chunks of 48 kHz stereo S16LE after gain processing.
It converts to mono, resamples to 16 kHz, retains 600 ms of model context, and
uses RVC with RMVPE pitch extraction, a 40 ms overlap, and a 10 ms alignment
search. Retrieval index mixing is disabled for this baseline. Output is
resampled to 48 kHz stereo and clamped to 0.95 full scale.

The daemon continues publishing the same virtual microphone while the worker
loads, fails, or is switched off. Conversion errors produce silence, never raw
input fallback while conversion is enabled. Request and response queues each
hold one chunk. Playback is bounded and rejects samples older than 650 ms.
Source, pitch, gain-mode, and mute changes invalidate pending results and reset
conversion history. Final mute gates the PCM after inference, immediately before
writing to the virtual microphone. Turning conversion off frees its GPU model.

Controls appear under Audio → Output: Voice conversion On/Off and Pitch (-12 to
+12 semitones). Enabling conversion does not unmute output. Output itself must
be on for the conversion worker to run. Settings are persisted in the existing
isolated audio preferences. No default microphone is changed by this profile.

`Pipeline delay` measures elapsed time from a PCM block entering the daemon to
its publication. It excludes hardware, PulseAudio capture buffering, downstream
application buffers, and acoustic playback. It is not total microphone-to-ear
latency. Dropped chunks and inference time are shown separately.

## Initial benchmark

`tools/voice-rvc --benchmark` uses synthetic tone input, opens no audio device,
and performs warmup followed by nine measurements. On the RTX 3070, the first
warmup took about 10.3 s; subsequent 160 ms chunks took about 35–40 ms and peak
Torch allocation was about 448 MiB. These are inference-only measurements with
video processing available, not proof of speech quality or total latency.

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

## Current French trial

The default model is [French Woman by DantSu](https://github.com/DantSu/RVC-french-woman-model),
release 0.0.1 (`Beatrice-Harvest.pth`, RVC v2, 48 kHz). The publisher describes
it as a French female voice trained with Harvest. Inference still uses RMVPE,
the same pitch setting, and no retrieval index to compare the model alone.
The release archive and extracted checkpoint are SHA-256 verified; the large
retrieval index is not extracted. The public release does not document a license
or detailed corpus provenance; this repository does not redistribute its weights
or claim rights for redistribution.

A synthetic benchmark with the original voice and camera running measured
35–64 ms per 160 ms chunk, with 451 MiB peak Torch allocation. Listening quality
is not inferred from these timings. A live camera/voice check measured a median
37 ms inference time with no additional dropped chunks over ten seconds.
The voice-worker-only model replacement retained the existing virtual source.
Before speech filtering, digital silence at +12 semitones produced approximately
-53 to -43 dBFS output with the French model (Shigure: about -90 dBFS).
Speech filtering and envelope matching now suppress this generated output. The first Japanese trial produced background
voice/echo artifacts and a slight accent according to the user.

## Original demo voice and attribution

The original model is **Shigure Tokina / 刻鳴時雨 (CV: Marukoro / 丸ころ)**,
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
It converts to mono, applies RNNoise and a speech gate, resamples to 16 kHz,
retains 600 ms of model context, and
uses RVC with RMVPE pitch extraction, a 40 ms overlap, and a 10 ms alignment
search. Retrieval index mixing is disabled for this baseline. Output is
matched to the cleaned input volume envelope before overlap alignment, then
published as 48 kHz stereo clamped to 0.95 full scale.

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
application buffers, and acoustic playback. It also does not track the signal
alignment inside RNNoise, the 40 ms speech-gate buffer, or RVC overlap alignment. It is not total microphone-to-ear
latency. Dropped chunks and inference time are shown separately.

## Initial benchmark

`tools/voice-rvc --benchmark` uses synthetic tone input, opens no audio device,
and performs warmup followed by nine measurements. On the RTX 3070, the first
warmup took about 10.3 s; subsequent 160 ms chunks took about 35–40 ms and peak
Torch allocation was about 448 MiB. These are inference-only measurements with
video processing available, not proof of speech quality or total latency.


## Speech filtering

The voice worker uses [RNNoise](https://github.com/xiph/rnnoise) through the pinned
`pyrnnoise==0.4.3` wheel (RNNoise BSD license; Python wrapper Apache-2.0).
Processing is local on CPU, in 10 ms mono frames, with no additional audio device
or GPU model. The filter is always active when this experimental RVC worker is
used. Raw output when voice conversion is off is unaffected.

Two consecutive frames with speech probability at least 0.65 open the gate.
A 0.35 continuation threshold and 180 ms hold preserve gaps and word endings;
a 40 ms delay retains audio preceding detection. Five-millisecond ramps smooth
gate transitions. RNNoise continues analyzing input while the gate is closed.
Resetting the conversion flushes denoiser state, delayed audio, and gate state.
This is speech detection, not speaker identification: other people speaking may
still pass, and actual desk taps mixed with speech require listening validation.

The converted signal follows the filtered input's 40 ms RMS envelope before SOLA,
following upstream RVC's volume matching. This prevents the model's generated
noise from filling gated silences. Final daemon mute remains unchanged. Automatic
gain should stay off for this trial: the user confirmed fewer artifacts without
it, and gain up to +24 dB was observed amplifying the input before conversion.

Validation: four CPU regression tests cover impact/noise rejection, reset,
chunk continuity, and envelope matching (`voice/.venv/bin/python -m unittest
discover -s voice -p 'test_*.py'`). With the French model at +12 semitones,
full GPU conversion produced digital zero for silence and three synthetic damped
noise impacts. The local ALSA `Front_Center.wav` speech fixture remained nonzero
(output RMS 0.048 full scale), with median processing around 47 ms and maximum
steady processing around 63 ms per 160 ms chunk while the live instance ran.
These are bounded signal checks, not proof that every real noise is rejected,
that speech quality is unchanged, or that latency equals Google Meet.


## Model selection and import

Audio → Output provides a model selector and an **Import .pth** button. The
profile's `audio.voice_models_dir` points to the isolated model directory.
Both installed demo voices are listed; importing a local RVC checkpoint adds it
to the list without selecting it. Uploads are limited to 128 MiB, accept regular
`.pth` filenames, and never overwrite installed files. ZIP archives and optional
retrieval indexes are not supported by this import control. Compatibility is
checked when the worker loads the selected model; a failed load stays silent
and reports an error so another model can be selected.

Selection is persisted as `audio.voice_model` in this worktree's settings.
Switching flushes buffered conversion audio and starts a new voice worker while
the virtual microphone stays published. Loading produces silence, not raw voice.
Mute, pitch, selected input, speech filtering and gain settings are preserved.
A generation counter prevents an obsolete worker from reporting readiness after
rapid selection changes, including selecting the original model again.

API: `GET /api/v1/audio/voice/models` lists installed models;
`POST /api/v1/audio/voice/models?name=Example.pth` imports the raw checkpoint body.
`POST /api/v1/audio/voice` accepts `enabled`, `pitch`, and optional `model` (the
installed filename). Omitting `model` preserves the current selection. These
routes use the daemon's existing authentication rules. Imported checkpoints are
local to this worktree and are not committed or uploaded to another service.


Model-management validation: Rust API/filesystem tests cover import without
activation, missing/path-traversal rejection, non-overwrite publication, symlink
exclusion, persisted selection, and obsolete-worker readiness. A headless Chrome
check exercised both demo selections, a 55 MB checkpoint import through the file
picker, selection of that imported voice, incompatible-checkpoint error, and
recovery to French Woman. The virtual-source ID and output mute were preserved
through these switches; temporary validation models were removed. The user also
confirmed that the preceding speech-filter change removed parasitic voices in
their setup.

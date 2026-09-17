# Licenses and model inventory

Reviewed on 2026-09-17. This inventory distinguishes files distributed in this
source repository from software and weights downloaded during installation.
It is not a complete license bill of materials for a compiled distribution.

## Original project work

Original Tarsier code and documentation are licensed under either
[MIT](MIT.txt) or [Apache-2.0](APACHE-2.0.txt), at the recipient's option
(`MIT OR Apache-2.0`). Existing third-party notices take precedence for the
material they cover; this choice does not relicense dependencies or models.

The bundled logos, mute images, Pixel Party background (PNG and BGR), and kelp
shader package are project assets covered by the same choice, to the extent of
the contributors' rights. The fictional LivePortrait demo portrait also has an
explicit permission and generation-provenance statement in
[assets/avatars/README.md](../assets/avatars/README.md).

## Bundled or adapted third-party code

| Component | Scope and upstream source | Retained notice |
| --- | --- | --- |
| Lucide 1.41.0 | Icons bundled in `web/vendor/lucide.js`; ISC, with MIT notices for icons derived from Feather. | [Lucide and Feather notices](../web/vendor/lucide-LICENSE) |
| LivePortrait | `modules/` and `models.yaml` vendored from [commit 9b294b3d0536135442ea73cb01e6cb3ca7029dd3](https://github.com/KlingAIResearch/LivePortrait/tree/9b294b3d0536135442ea73cb01e6cb3ca7029dd3); MIT. | [Upstream notice](../worker/src/tarsier_perception/liveportrait/THIRD_PARTY_LICENSE) and [integration provenance](../worker/src/tarsier_perception/liveportrait/README.md) |
| RVC WebUI | Real-time interface and overlap alignment adapted in `voice/worker.py` from [commit 81eed5e8f68b6bed1789f682fe78cdd324495afc](https://github.com/RVC-Project/Retrieval-based-Voice-Conversion-WebUI/tree/81eed5e8f68b6bed1789f682fe78cdd324495afc); MIT. The upstream runtime is downloaded separately. | [RVC copyright and MIT license](RVC-MIT.txt) |

Notices already colocated with bundled components remain there so those
components keep their attribution when packaged separately. New project license
texts and the RVC notice are collected in this directory.

## External models and optional runtimes

Weights are not committed. Exact download locations and checksums are recorded in
[models.py](../worker/src/tarsier_perception/models.py) and
[voice/assets.json](../voice/assets.json). A checksum identifies content; it does
not grant permission to use or redistribute it.

| Download or runtime | Terms and review status |
| --- | --- |
| MediaPipe gesture, face, pose, and selfie segmentation assets | Downloaded from Google's MediaPipe model storage. [MediaPipe code](https://github.com/google-ai-edge/mediapipe/blob/master/LICENSE) is Apache-2.0; this code license alone is not a model-specific redistribution review. Consult the corresponding Google model documentation before bundling the task/model files. |
| LivePortrait core human-animation weights | The [official model card](https://huggingface.co/KlingTeam/LivePortrait) declares MIT. Tarsier downloads only the five core animation/stitching checkpoints listed in `models.py`. |
| InsightFace | Not included, downloaded, or used by the current Tarsier integration. The retained [upstream LivePortrait notice](https://github.com/KlingAIResearch/LivePortrait/blob/main/LICENSE) restricts the separate InsightFace detection models to non-commercial research. |
| Depth Anything V2 Small HF | The [specific Small HF model card](https://huggingface.co/depth-anything/Depth-Anything-V2-Small-hf) declares Apache-2.0. This statement does not cover other model sizes. |
| RVC HuBERT and RMVPE weights | Downloaded from [lj1995/VoiceConversionWebUI](https://huggingface.co/lj1995/VoiceConversionWebUI). Weight-specific permissions have not been established by this review; do not infer them from RVC's MIT code license. |
| French Woman / Beatrice-Harvest | Optional voice setup downloads release 0.0.1 from [DantSu](https://github.com/DantSu/RVC-french-woman-model). No explicit model license was found in the reviewed publication. Redistribution and commercial-use permissions remain unresolved. |
| Shigure Tokina RVC | Optional voice setup also downloads this model. Follow the [original distributor's conditions](https://huggingface.co/yasyune/Shigure_Tokina_RVC) and [voice corpus guidelines](https://bindume-chan.booth.pm/items/3640133). Attribution is recorded in [voice/README.md](../voice/README.md); it is not a blanket permission for every use. |
| Whisper client | Downloads the model chosen with `--model` through faster-whisper. Review the chosen model's card and the runtime dependency licenses when distributing it. |

`tools/voice-setup` currently downloads both trial voices automatically when the
optional setup is invoked. Their unresolved or additional conditions are not
removed by downloading them directly from their publishers. The source release
does not represent the voice stack as cleared for unrestricted commercial use.

## Binary and environment distributions

Cargo, Python, npm, and system dependencies keep their own licenses. Lockfiles
identify the selected package versions; they do not replace license texts.
Before distributing binaries, containers, Python environments, model caches, or
installers containing third-party material, inventory what is actually included
and supply its required notices and any corresponding source obligations.
In particular, review the selected GStreamer/FFmpeg libraries and codecs and the
optional voice environment separately. This source-repository inventory does
not certify those future distributions.

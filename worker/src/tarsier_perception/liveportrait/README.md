# LivePortrait integration provenance

The files under `modules/` and `models.yaml` were vendored from
[`KlingAIResearch/LivePortrait`](https://github.com/KlingAIResearch/LivePortrait)
at commit `9b294b3d0536135442ea73cb01e6cb3ca7029dd3`. They are used by Tarsier's
minimal portrait-animation runtime and remain covered by the upstream MIT
license in `THIRD_PARTY_LICENSE`.

Tarsier downloads only these core human-animation weights from the official
`KlingTeam/LivePortrait` Hugging Face repository:

- appearance feature extractor;
- motion extractor;
- warping module;
- SPADE generator;
- stitching/retargeting module.

Every download is pinned by SHA-256 in `tarsier_perception.models`. Model files
are cached outside the repository.

No InsightFace code or model is included or downloaded by Tarsier. The current
runtime uses MediaPipe and does not implement an InsightFace fallback. The
upstream notice about non-commercial InsightFace detection models is retained
verbatim in `THIRD_PARTY_LICENSE`; it concerns those separate upstream assets.
Adding them would require a separate review of their terms.

The upstream LivePortrait model card declares MIT for its core animation
weights. See the [license inventory](../../../../licenses/README.md) for sources
and the distinction between Tarsier code, vendored code, and model downloads.

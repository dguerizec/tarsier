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

No InsightFace code or model is included. Upstream restricts the bundled
InsightFace detection models to non-commercial research use. Tarsier may keep
that detector as an explicit experimental fallback for this personal,
non-commercial research project, but MediaPipe is the default and any future
commercial use must exclude or separately relicense the InsightFace assets.

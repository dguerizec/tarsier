"""Compare observation delegates on identical local video frames without devices or HTTP."""

import argparse
import hashlib
import json
import time
from pathlib import Path

import cv2
import numpy as np
from tarsier_perception.delegates import Delegates
from tarsier_perception.worker import (
    MediaPipeDetector,
    MediaPipeSegmenter,
    PoseConstraintStore,
)

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--input", type=Path, required=True, help="Path to your local video recording"
)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument(
    "--stride", type=int, default=3, help="Process every Nth video frame"
)
args = parser.parse_args()
if args.stride < 1:
    parser.error("--stride must be positive")
if not args.input.is_file():
    parser.error(
        f"Local fixture is missing: {args.input}; see docs/perception-testing.md"
    )
if args.output.exists():
    parser.error("Output already exists; choose a new path")
with args.input.open("rb") as file:
    digest = hashlib.file_digest(file, "sha256").hexdigest()
model_dir = Path.home() / ".cache/tarsier/models"
cpu_masks = {}
records = []
for backend in ("cpu", "gpu"):
    capture = cv2.VideoCapture(str(args.input))
    if not capture.isOpened():
        raise RuntimeError("Cannot open video fixture")
    fps = capture.get(cv2.CAP_PROP_FPS)
    if fps <= 0:
        raise RuntimeError("Video has no valid frame rate")
    constraints = PoseConstraintStore()
    strategy = Delegates(face=backend, hands=backend, pose=backend)
    try:
        with (
            MediaPipeDetector(model_dir, 0.5, strategy) as detector,
            MediaPipeSegmenter(model_dir, constraints) as segmenter,
        ):
            index = -1
            while True:
                ok, frame = capture.read()
                if not ok:
                    break
                index += 1
                if index % args.stride:
                    continue
                frame = cv2.resize(frame, (640, 360))
                timestamp = round(index * 1000 / fps)
                wall_start, cpu_start = time.perf_counter(), time.process_time()
                face, hands, pose, gesture, confidence, mask = detector.detect(
                    frame, timestamp
                )
                constraints.update(mask)
                foreground = segmenter.segment(frame, timestamp)
                record = {
                    "backend": backend,
                    "frame": index,
                    "timestamp_ms": timestamp,
                    "face_landmarks": len(face),
                    "hand_landmarks": len(hands),
                    "pose_landmarks": len(pose),
                    "gesture": gesture,
                    "confidence": confidence,
                    "pose_foreground_pixels": int((mask > 127).sum()),
                    "constrained_foreground_pixels": int((foreground > 127).sum()),
                    "wall_ms": (time.perf_counter() - wall_start) * 1000,
                    "process_cpu_ms": (time.process_time() - cpu_start) * 1000,
                }
                binary = mask > 127
                if backend == "cpu":
                    cpu_masks[index] = np.packbits(binary)
                else:
                    control = (
                        np.unpackbits(cpu_masks[index])
                        .reshape(binary.shape)
                        .astype(bool)
                    )
                    union = int((control | binary).sum())
                    record["pose_mask_iou"] = (
                        float((control & binary).sum() / union) if union else None
                    )
                records.append(record)
    finally:
        capture.release()
    print(
        f"{backend}: {sum(r['backend'] == backend for r in records)} frames", flush=True
    )
summary = {}
for backend in ("cpu", "gpu"):
    rows = [r for r in records if r["backend"] == backend]
    summary[backend] = {
        "frames": len(rows),
        "frames_with_face": sum(r["face_landmarks"] > 0 for r in rows),
        "frames_with_hands": sum(r["hand_landmarks"] > 0 for r in rows),
        "frames_with_pose": sum(r["pose_landmarks"] > 0 for r in rows),
        "pose_detected_but_empty_mask": sum(
            r["pose_landmarks"] > 0 and r["pose_foreground_pixels"] == 0 for r in rows
        ),
        "gestures": {
            g: sum(r["gesture"] == g for r in rows)
            for g in sorted({r["gesture"] for r in rows if r["gesture"]})
        },
        "mean_wall_ms": float(np.mean([r["wall_ms"] for r in rows])),
        "mean_process_cpu_ms": float(np.mean([r["process_cpu_ms"] for r in rows])),
    }
ious = [r["pose_mask_iou"] for r in records if r.get("pose_mask_iou") is not None]
summary["gpu"]["median_pose_mask_iou"] = float(np.median(ious)) if ious else None
args.output.parent.mkdir(parents=True, exist_ok=True)
with args.output.open("x") as file:
    json.dump(
        {
            "input": str(args.input),
            "sha256": digest,
            "stride": args.stride,
            "note": (
                "Offline sampled-frame functional comparison; not full pipeline telemetry. "
                "CPU is a reference, not ground truth. Audio is not processed."
            ),
            "summary": summary,
            "frames": records,
        },
        file,
        indent=2,
    )
print(json.dumps(summary, indent=2))

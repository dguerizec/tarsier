"""Validate an exported portrait and generate a neutral, local library preview."""

from __future__ import annotations

import argparse
import json
import os
import resource
import zipfile
from pathlib import Path

import cv2

from .avatar_motion import AvatarMotion
from .portrait3d import Portrait3DAvatarEngine, load_asset

MAX_MESH_BYTES = 512 * 1024 * 1024


def validate_model(directory: Path) -> None:
    # Bound decompression before NumPy reads the untrusted NPZ triangle streams.
    with zipfile.ZipFile(directory / "mesh.npz") as archive:
        entries = archive.infolist()
        if not entries or len(entries) > 128 or sum(e.file_size for e in entries) > MAX_MESH_BYTES:
            raise ValueError("model mesh is too large")
        if any(e.is_dir() or Path(e.filename).name != e.filename for e in entries):
            raise ValueError("invalid mesh archive")
    manifest, _ = load_asset(directory)
    for draw in manifest["draws"]:
        if texture := draw.get("texture"):
            image = cv2.imread(str(directory / texture), cv2.IMREAD_COLOR)
            if image is None or max(image.shape[:2]) > 8192:
                raise ValueError("model texture is invalid or exceeds 8192 pixels")


def prepare_model(directory: Path, preview: Path) -> dict:
    validate_model(directory)
    try:
        with Portrait3DAvatarEngine(directory, 256, 256) as engine:
            pixels = engine.render(AvatarMotion())
        if not cv2.imwrite(str(preview), pixels):
            raise RuntimeError("could not save preview")
        preview.chmod(0o600)
    except Exception:
        preview.unlink(missing_ok=True)
        return {
            "preview": False,
            "warning": "Model imported, but its preview could not be generated.",
        }
    return {"preview": True, "warning": None}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("preview", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    resource.setrlimit(resource.RLIMIT_CPU, (60, 60))
    resource.setrlimit(resource.RLIMIT_DATA, (2 * 1024**3, 2 * 1024**3))
    try:
        result = prepare_model(args.directory, args.preview)
    except Exception:
        print(json.dumps({
            "error": "Invalid Personal 3D export. Check the manifest, mesh and textures.",
        }))
        raise SystemExit(1) from None
    print(json.dumps(result))


if __name__ == "__main__":
    main()

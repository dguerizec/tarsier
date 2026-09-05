from __future__ import annotations

import argparse
import json
import logging
from pathlib import Path

from .models import default_model_dir, describe_models, download_models
from .worker import run_mock, run_worker


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Tarsier MediaPipe perception worker")
    parser.add_argument("--daemon-url", default="http://127.0.0.1:8742")
    parser.add_argument("--log-level", default="INFO")
    subparsers = parser.add_subparsers(dest="command", required=True)

    models = subparsers.add_parser("models", help="download or inspect model assets")
    models.add_argument("--model-dir", type=Path, default=default_model_dir())
    models.add_argument("--download", action="store_true")
    models.add_argument("--force", action="store_true")
    models.add_argument("--avatar", action="store_true")

    serve = subparsers.add_parser("serve", help="process frames from MJPEG or a V4L2 source")
    serve.add_argument("--source", "--device", dest="source")
    serve.add_argument("--width", type=int, default=640)
    serve.add_argument("--height", type=int, default=360)
    serve.add_argument("--fps", type=float, default=10.0)
    serve.add_argument("--mask-fps", type=float, default=30.0)
    serve.add_argument("--minimum-confidence", type=float, default=0.5)
    serve.add_argument("--model-dir", type=Path, default=default_model_dir())
    serve.add_argument("--avatar-source", type=Path)
    serve.add_argument("--avatar-fps", type=float, default=15.0)
    serve.add_argument("--avatar-width", type=int, default=1280)
    serve.add_argument("--avatar-height", type=int, default=720)
    serve.add_argument("--avatar-compile", action="store_true")

    mock = subparsers.add_parser("mock", help="publish deterministic synthetic observations")
    mock.add_argument("--fps", type=float, default=10.0)
    mock.add_argument("--open-palm", action="store_true")
    return parser


def main() -> None:
    args = build_parser().parse_args()
    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    if args.command == "models":
        if args.download:
            download_models(args.model_dir, force=args.force, include_avatar=args.avatar)
        print(json.dumps(describe_models(args.model_dir, include_avatar=args.avatar), indent=2))
        return
    if args.command == "mock":
        run_mock(args.daemon_url, args.fps, args.open_palm)
        return
    if not 0.0 <= args.minimum_confidence <= 1.0:
        raise SystemExit("--minimum-confidence must be between 0 and 1")
    if args.fps <= 0:
        raise SystemExit("--fps must be greater than zero")
    if args.mask_fps <= 0:
        raise SystemExit("--mask-fps must be greater than zero")
    if args.avatar_fps <= 0:
        raise SystemExit("--avatar-fps must be greater than zero")
    if args.avatar_width <= 0 or args.avatar_height <= 0:
        raise SystemExit("--avatar-width and --avatar-height must be greater than zero")
    if args.avatar_source is not None and not args.avatar_source.is_file():
        raise SystemExit(f"avatar source image does not exist: {args.avatar_source}")
    unavailable = [
        model
        for model in describe_models(
            args.model_dir, include_avatar=args.avatar_source is not None
        )
        if not model["verified"]
    ]
    if unavailable:
        raise SystemExit(
            "worker models are missing or invalid; run `tarsier-perception models --download` "
            "with `--avatar` when avatar output is enabled"
        )
    try:
        source = args.source or f"{args.daemon_url.rstrip('/')}/api/v1/perception/input.mjpeg"
        run_worker(
            source=source,
            width=args.width,
            height=args.height,
            fps=args.fps,
            mask_fps=args.mask_fps,
            daemon_url=args.daemon_url,
            model_dir=args.model_dir,
            minimum_confidence=args.minimum_confidence,
            avatar_source=args.avatar_source,
            avatar_fps=args.avatar_fps,
            avatar_width=args.avatar_width,
            avatar_height=args.avatar_height,
            avatar_compile=args.avatar_compile,
        )
    except KeyboardInterrupt:
        logging.getLogger(__name__).info("perception worker stopped")


if __name__ == "__main__":
    main()

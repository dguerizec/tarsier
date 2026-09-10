"""Read the daemon's latest BGR frame from a read-only anonymous shared buffer.

A notification is only a wakeup: sequence numbers identify frames. Copy pixels
under flock before handing them to asynchronous model threads. This deliberately
is not a zero-copy lifetime claim; it removes JPEG codecs and HTTP image bodies.
"""

from __future__ import annotations

import contextlib
import fcntl
import mmap
import os
import re
import socket
import struct
import time
from urllib.parse import urlsplit

import cv2
import numpy as np

from .telemetry import stages

HEADER = struct.Struct("<8sIIIIQQQQ8x")
MAX_BYTES = 64 * 1024 * 1024


def capture_shared_frames(source: str, width: int, height: int):
    uri = urlsplit(source)
    if (
        uri.scheme != "shm"
        or uri.netloc
        or uri.query
        or not re.fullmatch(r"/proc/[0-9]+/fd/[0-9]+", uri.path)
        or not re.fullmatch(r"[0-9a-f]{32}", uri.fragment)
    ):
        raise RuntimeError("invalid shared frame source")
    with open(uri.path, "rb", buffering=0) as stream:
        size = os.fstat(stream.fileno()).st_size
        if not HEADER.size < size <= HEADER.size + MAX_BYTES:
            raise RuntimeError("invalid shared frame buffer size")
        with (
            mmap.mmap(stream.fileno(), size, access=mmap.ACCESS_READ) as buffer,
            socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as notification,
        ):
            notification.bind("\0tarsier-" + uri.fragment)
            notification.settimeout(1.0)
            previous = None
            last_frame_at = time.monotonic()
            while True:
                frame = None
                # The writer uses a nonblocking exclusive lock; it skips a
                # frame if this short copy is in progress, never queues video.
                fcntl.flock(stream.fileno(), fcntl.LOCK_SH)
                try:
                    magic, w, h, stride, rotation, sequence, frame_id, captured_at, length = (
                        HEADER.unpack_from(buffer)
                    )
                    if (
                        magic != b"TARSFRM1"
                        or w == 0
                        or h == 0
                        or stride != (w * 3 + 3) & ~3
                        or stride * h != size - HEADER.size
                        or rotation not in (0, 90, 180, 270)
                        or length not in (0, size - HEADER.size)
                    ):
                        raise RuntimeError("invalid shared frame header")
                    if sequence != previous:
                        previous = sequence
                        if length:
                            if not frame_id or not captured_at:
                                raise RuntimeError("missing shared frame provenance")
                            with stages.measure("input_copy"):
                                view = np.ndarray(
                                    (h, w, 3),
                                    dtype=np.uint8,
                                    buffer=buffer,
                                    offset=HEADER.size,
                                    strides=(stride, 3, 1),
                                )
                                frame = view.copy()
                                del view
                finally:
                    fcntl.flock(stream.fileno(), fcntl.LOCK_UN)
                if frame is not None:
                    if frame.shape[:2] != (height, width):
                        with stages.measure("input_copy"):
                            frame = cv2.resize(
                                frame, (width, height), interpolation=cv2.INTER_LINEAR
                            )
                    last_frame_at = time.monotonic()
                    yield frame_id, captured_at, rotation, frame
                if time.monotonic() - last_frame_at > 5.0:
                    raise RuntimeError("shared frame producer stopped publishing")
                with contextlib.suppress(TimeoutError):
                    notification.recv(1)

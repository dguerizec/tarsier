"""Protocol, ownership and wakeup regression checks for the local frame transport."""

import contextlib
import fcntl
import mmap
import os
import socket
import tempfile
import uuid

import numpy as np
import pytest

from tarsier_perception.shared_frames import HEADER, capture_shared_frames


class Producer:
    def __init__(self, width=3, height=2):
        self.width, self.height = width, height
        self.stride = (width * 3 + 3) & ~3
        self.file = tempfile.TemporaryFile()  # noqa: SIM115 - owned by the fixture through close()
        self.fd = self.file.fileno()
        os.ftruncate(self.fd, HEADER.size + self.stride * height)
        self.buffer = mmap.mmap(self.fd, 0)
        self.nonce = uuid.uuid4().hex
        self.source = f"shm:///proc/{os.getpid()}/fd/{self.fd}#{self.nonce}"
        self.sequence = 0
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)

    def publish(self, value, frame_id=1, captured_at=1000, rotation=0):
        self.sequence += 1
        pixels = np.full((self.height, self.stride), 238, dtype=np.uint8)
        pixels[:, : self.width * 3] = value
        fcntl.flock(self.fd, fcntl.LOCK_EX)
        try:
            self.buffer[HEADER.size :] = pixels.tobytes()
            self.buffer[: HEADER.size] = HEADER.pack(
                b"TARSFRM1",
                self.width,
                self.height,
                self.stride,
                rotation,
                self.sequence,
                frame_id,
                captured_at,
                pixels.nbytes,
            )
        finally:
            fcntl.flock(self.fd, fcntl.LOCK_UN)
        with contextlib.suppress(ConnectionRefusedError):
            self.socket.sendto(b"1", "\0tarsier-" + self.nonce)

    def close(self):
        self.socket.close()
        self.buffer.close()
        self.file.close()


@pytest.fixture
def producer():
    value = Producer()
    try:
        yield value
    finally:
        value.close()


def test_shared_reader_preserves_pixels_provenance_and_private_frame_ownership(producer):
    producer.publish(17, frame_id=9, captured_at=123456, rotation=90)
    frames = capture_shared_frames(producer.source, 3, 2)
    try:
        frame_id, captured_at, rotation, first = next(frames)
        assert (frame_id, captured_at, rotation) == (9, 123456, 90)
        assert first.shape == (2, 3, 3)
        assert first.flags.c_contiguous
        assert np.all(first == 17)
        # A source switch may reset frame IDs; sequence remains authoritative.
        producer.publish(33, frame_id=1, captured_at=123500, rotation=270)
        frame_id, captured_at, rotation, second = next(frames)
        assert (frame_id, captured_at, rotation) == (1, 123500, 270)
        assert np.all(first == 17), "asynchronous consumers must retain immutable frame contents"
        assert np.all(second == 33)
        producer.publish(44, frame_id=2)
        producer.publish(55, frame_id=3)
        assert np.all(next(frames)[3] == 55), "slow consumers must skip old frames"
    finally:
        frames.close()


def test_shared_reader_reconnects_and_resizes(producer):
    producer.publish(90)
    for _ in range(2):
        frames = capture_shared_frames(producer.source, 6, 4)
        try:
            frame = next(frames)[3]
            assert frame.shape == (4, 6, 3)
            assert np.all(frame == 90)
        finally:
            frames.close()


def test_shared_reader_rejects_malformed_header_and_uri(producer):
    producer.publish(0)
    producer.buffer[0] = 0
    with pytest.raises(RuntimeError, match="header"):
        next(capture_shared_frames(producer.source, 3, 2))
    for uri in ("shm:///etc/passwd", "shm://remote/proc/1/fd/2#" + "a" * 32):
        with pytest.raises(RuntimeError, match="source"):
            next(capture_shared_frames(uri, 3, 2))


def test_producer_timeout_closes_reader_resources(producer, monkeypatch):
    from types import SimpleNamespace

    from tarsier_perception import shared_frames

    clock = iter([0.0, 0.0, 6.0])
    monkeypatch.setattr(shared_frames, "time", SimpleNamespace(monotonic=lambda: next(clock)))
    producer.publish(17)
    frames = capture_shared_frames(producer.source, 3, 2)
    next(frames)
    with pytest.raises(RuntimeError, match="producer stopped"):
        next(frames)
    # An exception must release the abstract address as well as the mapping.
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as probe:
        probe.bind("\0tarsier-" + producer.nonce)

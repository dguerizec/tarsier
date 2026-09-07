"""Fixed-size PCM bridge to the pinned upstream RVC real-time engine.

Protocol: startup b'RVC1'; requests <iI (pitch, reset) + 160 ms stereo S16LE;
responses <f (inference milliseconds) + the same number of PCM bytes.
Upstream RVC is MIT licensed; the overlap alignment follows its realtime GUI.
"""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import struct
import sys
import time
from types import SimpleNamespace

FRAMES = 7680
BLOCK_BYTES = FRAMES * 4


class Converter:
    def __init__(self, upstream: Path, model: Path):
        os.environ.setdefault("OMP_NUM_THREADS", "2")
        os.environ.setdefault("OPENBLAS_NUM_THREADS", "1")
        os.environ["RVC_CUDA_GRAPH"] = "0"
        os.chdir(upstream)
        sys.path.insert(0, str(upstream))
        import numpy as np
        import torch
        import torch.nn.functional as functional
        from torchaudio.transforms import Resample
        from infer.rtrvc import RVC

        self.np, self.torch, self.functional = np, torch, functional
        torch.set_num_threads(2)
        device = "cuda:0" if torch.cuda.is_available() else "cpu"
        self.rvc = RVC(0, 0.0, str(model), "", 0.0,
                       SimpleNamespace(device=device, is_half=device.startswith("cuda")))
        if getattr(self.rvc, "net_g", None) is None:
            raise RuntimeError("RVC model initialization failed")
        self.device = device
        self.resample_in = Resample(48000, 16000).to(device)
        self.resample_out = Resample(self.rvc.tgt_sr, 48000).to(device)
        self.audio = torch.zeros(28800 + 1920 + 480 + FRAMES, device=device)
        self.overlap = torch.zeros(1920, device=device)
        self.fade = torch.sin(torch.linspace(0, torch.pi / 2, 1920, device=device)) ** 2
        self.kernel = torch.ones(1, 1, 1920, device=device)

    def reset(self):
        self.audio.zero_()
        self.overlap.zero_()
        self.rvc.cache_pitch.zero_()
        self.rvc.cache_pitchf.zero_()

    def convert(self, pcm: bytes, pitch: int, reset: bool = False) -> tuple[bytes, float]:
        torch, np = self.torch, self.np
        if reset:
            self.reset()
        start = time.perf_counter()
        with torch.inference_mode():
            mono = np.frombuffer(pcm, dtype='<i2').reshape(-1, 2).mean(axis=1).astype(np.float32) / 32768
            self.audio[:-FRAMES] = self.audio[FRAMES:].clone()
            self.audio[-FRAMES:] = torch.from_numpy(mono).to(self.device)
            self.rvc.change_key(pitch)
            inferred = self.rvc.infer(self.resample_in(self.audio), FRAMES // 3, 60, 21, 'rmvpe')
            converted = self.resample_out(inferred.float())
            segment = converted[None, None, :2400]
            correlation = self.functional.conv1d(segment, self.overlap[None, None, :])
            energy = self.functional.conv1d(segment.square(), self.kernel).add(1e-8).sqrt()
            offset = int(torch.argmax(correlation / energy))
            converted = converted[offset:]
            converted[:1920] = converted[:1920] * self.fade + self.overlap * (1 - self.fade)
            self.overlap.copy_(converted[FRAMES:FRAMES + 1920])
            output = converted[:FRAMES].clamp(-0.95, 0.95).cpu().numpy()
        stereo = np.repeat((output * 32767).astype('<i2')[:, None], 2, axis=1).tobytes()
        if len(stereo) != BLOCK_BYTES or not np.isfinite(output).all():
            raise RuntimeError('Invalid converted PCM block')
        return stereo, (time.perf_counter() - start) * 1000


def read_exact(stream, count):
    data = bytearray()
    while len(data) < count:
        block = stream.read(count - len(data))
        if not block:
            raise EOFError
        data.extend(block)
    return bytes(data)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--upstream', type=Path, required=True)
    parser.add_argument('--model', type=Path, required=True)
    parser.add_argument('--benchmark', action='store_true')
    args = parser.parse_args()
    upstream, model = args.upstream.resolve(), args.model.resolve()
    output = sys.stdout.buffer
    sys.stdout = sys.stderr
    converter = Converter(upstream, model)
    # Warm all inference paths before publishing ready. No device is opened here.
    np = converter.np
    tone = (np.sin(np.arange(FRAMES) * 2 * np.pi * 180 / 48000) * 3000).astype('<i2')
    probe = np.repeat(tone[:, None], 2, axis=1).tobytes()
    timings = []
    for _ in range(12 if args.benchmark else 3):
        _, elapsed = converter.convert(probe, 0)
        timings.append(elapsed)
    converter.reset()
    if args.benchmark:
        import json
        print(json.dumps({'block_ms': 160, 'device': converter.device,
                          'warmup_ms': timings[:3], 'steady_ms': timings[3:],
                          'gpu_peak_mb': converter.torch.cuda.max_memory_allocated() / 2**20
                          if converter.device.startswith('cuda') else None}))
        return
    output.write(b'RVC1')
    output.flush()
    while True:
        try:
            header = read_exact(sys.stdin.buffer, 8)
            pcm = read_exact(sys.stdin.buffer, BLOCK_BYTES)
        except EOFError:
            return
        pitch, reset = struct.unpack('<iI', header)
        if not -12 <= pitch <= 12:
            raise ValueError('Pitch must be between -12 and 12 semitones')
        converted, elapsed = converter.convert(pcm, pitch, bool(reset))
        output.write(struct.pack('<f', elapsed) + converted)
        output.flush()


if __name__ == '__main__':
    main()

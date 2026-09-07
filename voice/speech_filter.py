"""Local RNNoise suppression and a delayed speech gate for 48 kHz mono audio."""
from collections import deque

import numpy as np
from pyrnnoise import rnnoise

FRAME = 480
LOOKAHEAD = 4  # 40 ms lets the gate retain consonants before confident speech.
HOLD = 18  # Preserve short gaps and word endings for 180 ms.


class SpeechFilter:
    def __init__(self):
        self.state = None
        self.reset()

    def close(self):
        if self.state is not None:
            rnnoise.destroy(self.state)
            self.state = None

    def __del__(self):
        self.close()

    def reset(self):
        self.close()
        self.state = rnnoise.create()
        if not self.state:
            raise RuntimeError('Could not initialize RNNoise')
        self.pending = deque(np.zeros(FRAME, dtype=np.float32) for _ in range(LOOKAHEAD))
        self.confident = 0
        self.hold = 0
        self.gain = 0.0

    def process(self, mono):
        if mono.ndim != 1 or len(mono) % FRAME:
            raise ValueError('Speech filter requires complete 10 ms mono frames')
        output = np.empty_like(mono, dtype=np.float32)
        for start in range(0, len(mono), FRAME):
            pcm = np.rint(np.clip(mono[start:start + FRAME], -1, 1) * 32767).astype(np.int16)
            cleaned, probability = rnnoise.process_mono_frame(self.state, pcm)
            if not np.isfinite(probability):
                raise RuntimeError('Invalid RNNoise speech probability')
            self.confident = self.confident + 1 if probability >= 0.65 else 0
            if self.confident >= 2 or (self.hold and probability >= 0.35):
                self.hold = HOLD
            else:
                self.hold = max(0, self.hold - 1)
            self.pending.append(cleaned.astype(np.float32) / 32768)
            delayed = self.pending.popleft()
            target = float(self.hold > 0)
            # A 5 ms ramp avoids discontinuities at gate boundaries.
            envelope = np.full(FRAME, target, dtype=np.float32)
            envelope[:240] = np.linspace(self.gain, target, 240)
            self.gain = target
            output[start:start + FRAME] = delayed * envelope
        return output

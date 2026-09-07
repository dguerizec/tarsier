"""Regression checks for speech preservation, transient rejection, and resets."""
import unittest

import numpy as np
import torch
import torch.nn.functional as functional

from speech_filter import SpeechFilter
from worker import match_envelope


class SpeechFilterTests(unittest.TestCase):
    def test_impacts_and_stationary_noise_do_not_open_gate(self):
        rng = np.random.default_rng(123)
        impacts = np.zeros(144000, dtype=np.float32)
        for start in (24000, 48000, 96000):
            impacts[start:start + 2400] = rng.normal(size=2400) * np.exp(-np.arange(2400) / 300) * .4
        for signal in (impacts, rng.normal(0, .01, 144000).astype(np.float32)):
            with self.subTest(peak=float(abs(signal).max())):
                processor = SpeechFilter()
                self.assertEqual(np.count_nonzero(processor.process(signal)), 0)
                processor.close()

    def test_reset_discards_delayed_audio_and_open_gate(self):
        processor = SpeechFilter()
        processor.pending[-1][:] = .5
        processor.hold = 18
        processor.gain = 1
        processor.reset()
        self.assertEqual(np.count_nonzero(processor.process(np.zeros(7680, np.float32))), 0)
        processor.close()

    def test_chunk_boundaries_do_not_change_filter_output(self):
        signal = np.random.default_rng(42).normal(0, .05, 15360).astype(np.float32)
        whole, split = SpeechFilter(), SpeechFilter()
        np.testing.assert_array_equal(whole.process(signal),
                                      np.concatenate([split.process(signal[:7680]), split.process(signal[7680:])]))
        whole.close()
        split.close()

    def test_envelope_removes_generated_sound_on_silence_without_muting_speech(self):
        generated = torch.full((10080,), .1)
        self.assertEqual(torch.count_nonzero(match_envelope(torch.zeros_like(generated), generated, functional)), 0)
        speech = torch.full_like(generated, .025)
        torch.testing.assert_close(match_envelope(speech, generated, functional), speech)
        self.assertTrue(torch.isfinite(match_envelope(speech, torch.zeros_like(generated), functional)).all())


if __name__ == '__main__':
    unittest.main()

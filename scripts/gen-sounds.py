#!/usr/bin/env python3
"""Writes the four notification sounds to crates/clusia/assets/sounds/<id>.aiff.

Every sample is computed here from sine partials and a smooth envelope, so the files carry no
third-party audio. Deterministic: running it twice produces identical bytes. 16-bit mono AIFF
at 44.1 kHz, at most 0.6 s, peak at -10 dBFS (quieter than the macOS alert sounds).
"""
import math
import struct
import sys
from pathlib import Path

RATE = 44100
PEAK = 0.32


def extended(rate):
    """The 80-bit IEEE extended float AIFF uses for the sample rate."""
    exponent = 16383 + int(math.floor(math.log2(rate)))
    mantissa = int(rate * (2 ** (63 - int(math.floor(math.log2(rate))))))
    return struct.pack(">HQ", exponent, mantissa)


def tone(freq, start, length, decay, partials=((1.0, 1.0),)):
    """A struck tone: sine partials with an exponential decay and a 6 ms attack."""
    out = []
    for n in range(int(length * RATE)):
        t = n / RATE
        attack = min(1.0, t / 0.006)
        env = attack * math.exp(-t * decay)
        value = sum(a * math.sin(2 * math.pi * freq * m * t) for m, a in partials)
        out.append((start + t, env * value))
    return out


def mix(notes, seconds):
    total = int(seconds * RATE)
    samples = [0.0] * total
    for note in notes:
        for when, value in note:
            i = int(when * RATE)
            if i < total:
                samples[i] += value
    fade = int(0.03 * RATE)
    for i in range(fade):
        samples[total - 1 - i] *= i / fade
    top = max(abs(s) for s in samples) or 1.0
    return [s / top * PEAK for s in samples]


SOUNDS = {
    "leaf": lambda: mix(
        [
            tone(587.33, 0.00, 0.30, 7.0, ((1, 1.0), (2, 0.18))),
            tone(880.00, 0.11, 0.38, 6.0, ((1, 1.0), (2, 0.12))),
        ],
        0.5,
    ),
    "drop": lambda: mix([tone(740.0, 0.0, 0.42, 9.0, ((1, 1.0), (3, 0.08)))], 0.45),
    "chime": lambda: mix(
        [tone(1046.5, 0.0, 0.58, 5.0, ((1, 1.0), (2.76, 0.25), (5.4, 0.08)))], 0.6
    ),
    "tick": lambda: mix([tone(1760.0, 0.0, 0.12, 38.0, ((1, 1.0), (2, 0.3)))], 0.14),
}


def aiff(samples):
    pcm = b"".join(struct.pack(">h", int(round(s * 32767))) for s in samples)
    comm = struct.pack(">hIh", 1, len(samples), 16) + extended(RATE)
    ssnd = struct.pack(">II", 0, 0) + pcm
    body = b"AIFF"
    body += b"COMM" + struct.pack(">I", len(comm)) + comm
    body += b"SSND" + struct.pack(">I", len(ssnd)) + ssnd
    return b"FORM" + struct.pack(">I", len(body)) + body


def main():
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else (
        Path(__file__).resolve().parent.parent / "crates/clusia/assets/sounds"
    )
    out.mkdir(parents=True, exist_ok=True)
    for name, make in SOUNDS.items():
        samples = make()
        assert len(samples) / RATE <= 0.6, name
        (out / f"{name}.aiff").write_bytes(aiff(samples))
        print(f"{name}.aiff {len(samples) / RATE:.2f}s")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""engines.py — pluggable speech-to-text backends.

Whisper is the default, not a commitment. The brief flags accented, code-switched
audio as the thing most likely to kill this project, so the STT engine is the
component most likely to be replaced — which makes it the one part that should
not be wired into the pipeline by hand.

`transcribe.py` depends only on the `Transcriber` interface below. Adding an
engine means adding a class and one registry line; nothing downstream changes.

Every engine must return timestamped segments on a single timeline. That is not
negotiable: the whole two-stream speaker-attribution design is built on merging
by timestamp, so an engine that can only return a wall of text is unusable here
unless it is chunked first (see SarvamCloud).

    ./engines.py            list available engines and whether each is usable
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class Segment:
    """One timestamped utterance, in seconds relative to the start of the file."""

    start: float
    end: float
    text: str
    # Whisper-specific confidence signals. Engines that do not expose them leave
    # them at defaults, and the caller's waveform-energy filter still applies.
    no_speech_prob: float = 0.0
    avg_logprob: float = 0.0


class Transcriber:
    """The seam. Everything downstream depends on this and nothing else."""

    name = "base"
    #: False when the engine's dependency or credentials are missing.
    available = False
    #: True when audio leaves the machine. The project is local-first, so this
    #: is surfaced loudly rather than buried in a config file.
    cloud = False

    def transcribe(self, path: Path, language: str | None, vocab: str | None) -> list[Segment]:
        raise NotImplementedError

    def describe(self) -> str:
        return self.name


class MLXWhisper(Transcriber):
    """Whisper via mlx-whisper. Default: robust on accented and code-switched
    speech, which is exactly why the brief chose it over Parakeet."""

    cloud = False

    # mlx-community's repo names are not consistent — turbo has no `-mlx`
    # suffix, everything else does — so guessing from the size alone produces
    # 404s. Quantised sizes are listed because RAM headroom is scarce on this
    # machine; 8-bit large-v3 is smaller than fp16 turbo and is not distilled.
    ALIASES = {
        "large-v3-turbo": "mlx-community/whisper-large-v3-turbo",       # 1.61 GB
        "large-v3-turbo-q4": "mlx-community/whisper-large-v3-turbo-q4",  # 0.46 GB
        "large-v3": "mlx-community/whisper-large-v3-mlx",                # 3.08 GB
        "large-v3-8bit": "mlx-community/whisper-large-v3-mlx-8bit",      # 1.71 GB
        "large-v3-4bit": "mlx-community/whisper-large-v3-mlx-4bit",      # 1.95 GB
        "medium": "mlx-community/whisper-medium-mlx",                    # 1.52 GB
        "small": "mlx-community/whisper-small-mlx",
    }

    def __init__(self, model: str = "large-v3-turbo"):
        if "/" in model:
            self.model = model
        else:
            self.model = self.ALIASES.get(model, f"mlx-community/whisper-{model}-mlx")
        self.name = f"whisper:{model}"

    @property
    def available(self) -> bool:
        try:
            import mlx_whisper  # noqa: F401
        except ImportError:
            return False
        return True

    def transcribe(self, path, language, vocab):
        import mlx_whisper

        result = mlx_whisper.transcribe(
            str(path),
            path_or_hf_repo=self.model,
            language=language,
            task="transcribe",
            word_timestamps=True,
            # Sparse streams send this into repetition loops otherwise.
            condition_on_previous_text=False,
            initial_prompt=vocab,
            temperature=(0.0, 0.2, 0.4),
        )
        return [
            Segment(
                start=s["start"],
                end=s["end"],
                text=s["text"].strip(),
                no_speech_prob=s.get("no_speech_prob", 0.0),
                avg_logprob=s.get("avg_logprob", 0.0),
            )
            for s in result.get("segments", [])
        ]

    def describe(self):
        return f"{self.name} ({self.model})"


class FasterWhisper(Transcriber):
    """Whisper via CTranslate2. A genuinely different implementation of the same
    model family — useful for telling a model problem apart from a runtime bug."""

    cloud = False

    def __init__(self, model: str = "large-v3"):
        self.model = model
        self.name = f"faster-whisper:{model}"
        self._loaded = None

    @property
    def available(self) -> bool:
        try:
            import faster_whisper  # noqa: F401
        except ImportError:
            return False
        return True

    def transcribe(self, path, language, vocab):
        from faster_whisper import WhisperModel

        if self._loaded is None:
            self._loaded = WhisperModel(self.model, device="cpu", compute_type="int8")
        segments, _ = self._loaded.transcribe(
            str(path),
            language=language,
            task="transcribe",
            condition_on_previous_text=False,
            initial_prompt=vocab,
        )
        return [
            Segment(
                start=s.start,
                end=s.end,
                text=s.text.strip(),
                no_speech_prob=getattr(s, "no_speech_prob", 0.0),
                avg_logprob=getattr(s, "avg_logprob", 0.0),
            )
            for s in segments
        ]


class SarvamCloud(Transcriber):
    """Sarvam's Saarika/Saaras ASR — built specifically for Indian accents and
    code-mixed speech, which is our known risk area.

    This one sends audio off the machine, which breaks the project's local-only
    premise. It exists as a measuring stick: if Whisper mangles Hinglish and this
    does not, that quantifies what staying local actually costs. Requires
    SARVAM_API_KEY.

    UNTESTED — written from the API docs, never run against the live service.
    """

    cloud = True
    ENDPOINT = "https://api.sarvam.ai/speech-to-text"
    # The API returns a whole-file transcript, so we impose our own timeline by
    # cutting the audio into fixed windows. Coarser than Whisper's per-utterance
    # timestamps, but enough to merge two streams sensibly.
    CHUNK_SECONDS = 30

    def __init__(self, model: str = "saarika:v2"):
        self.model = model
        self.name = f"sarvam:{model}"

    @property
    def available(self) -> bool:
        return bool(os.environ.get("SARVAM_API_KEY"))

    def _post(self, chunk: Path, language: str | None) -> str:
        boundary = "----meetnote"
        fields = {"model": self.model}
        if language and language != "auto":
            # Sarvam wants BCP-47 style codes; English-India for our case.
            fields["language_code"] = "en-IN" if language == "en" else language
        body = bytearray()
        for key, value in fields.items():
            body += f"--{boundary}\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n".encode()
        body += (
            f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; "
            f"filename=\"{chunk.name}\"\r\nContent-Type: audio/wav\r\n\r\n"
        ).encode()
        body += chunk.read_bytes()
        body += f"\r\n--{boundary}--\r\n".encode()

        request = urllib.request.Request(
            self.ENDPOINT,
            data=bytes(body),
            headers={
                "api-subscription-key": os.environ["SARVAM_API_KEY"],
                "Content-Type": f"multipart/form-data; boundary={boundary}",
            },
        )
        with urllib.request.urlopen(request, timeout=300) as response:
            payload = json.load(response)
        return (payload.get("transcript") or "").strip()

    def transcribe(self, path, language, vocab):
        duration = float(
            subprocess.run(
                ["ffprobe", "-v", "error", "-show_entries", "format=duration",
                 "-of", "default=nw=1:nk=1", str(path)],
                capture_output=True, text=True, check=True,
            ).stdout.strip()
        )
        segments: list[Segment] = []
        with tempfile.TemporaryDirectory() as tmp:
            offset = 0.0
            while offset < duration:
                chunk = Path(tmp) / f"c{int(offset)}.wav"
                subprocess.run(
                    ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(path),
                     "-ss", str(offset), "-t", str(self.CHUNK_SECONDS),
                     "-ar", "16000", "-ac", "1", str(chunk)],
                    check=True,
                )
                text = self._post(chunk, language)
                if text:
                    segments.append(
                        Segment(start=offset, end=min(offset + self.CHUNK_SECONDS, duration), text=text)
                    )
                offset += self.CHUNK_SECONDS
        return segments


# ── registry ────────────────────────────────────────────────────────────────
# Adding an engine is one line here plus a class above. transcribe.py is
# untouched, which is the entire point of the exercise.

FACTORIES = {
    "whisper": MLXWhisper,
    "faster-whisper": FasterWhisper,
    "sarvam": SarvamCloud,
}

DEFAULT = "whisper:large-v3-turbo"


def build(spec: str) -> Transcriber:
    """`whisper`, `whisper:large-v3`, `sarvam:saarika:v2`, …"""
    name, _, variant = spec.partition(":")
    factory = FACTORIES.get(name)
    if factory is None:
        raise SystemExit(f"unknown engine '{name}'. Available: {', '.join(FACTORIES)}")
    engine = factory(variant) if variant else factory()
    if not engine.available:
        raise SystemExit(
            f"engine '{engine.name}' is not usable here.\n"
            f"  {'set SARVAM_API_KEY' if engine.cloud else 'install its package into the venv'}"
        )
    return engine


def status() -> list[tuple[str, bool, bool]]:
    rows = []
    for name, factory in FACTORIES.items():
        engine = factory()
        rows.append((name, engine.available, engine.cloud))
    return rows


if __name__ == "__main__":
    print("engine          usable  sends audio off-machine")
    for name, available, cloud in status():
        print(f"  {name:<14} {'yes' if available else 'no ':<7} {'YES' if cloud else 'no'}")
    print(f"\ndefault: {DEFAULT}")

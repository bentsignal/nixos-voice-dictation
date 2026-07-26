#!/usr/bin/env python3
"""Small, CPU-efficient Parakeet v3 sidecar for whisrs using sherpa-onnx."""

from __future__ import annotations

import argparse
import logging
import re
import subprocess
import tempfile
import wave
from contextlib import asynccontextmanager
from difflib import SequenceMatcher
from pathlib import Path

import numpy as np
import sherpa_onnx
from fastapi import FastAPI, File, Form, HTTPException, UploadFile

LOG = logging.getLogger("parakeet-sidecar")
WORD_RE = re.compile(r"[A-Za-z][A-Za-z0-9_.+/#-]{2,}")


class ScreenContext:
    """Best-effort KDE screenshot OCR and conservative name correction."""

    def __init__(self, enabled: bool, toggle_file: Path | None = None) -> None:
        self.enabled = enabled
        self.toggle_file = toggle_file

    def words(self) -> list[str]:
        enabled = self.toggle_file.exists() if self.toggle_file else self.enabled
        if not enabled:
            return []
        try:
            with tempfile.TemporaryDirectory(prefix="whisrs-ocr-") as directory:
                image = Path(directory) / "screen.png"
                subprocess.run(
                    ["spectacle", "--background", "--nonotify", "--fullscreen",
                     "--output", str(image)],
                    check=True, timeout=8, stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                result = subprocess.run(
                    ["tesseract", str(image), "stdout", "--psm", "11"],
                    check=True, timeout=15, capture_output=True, text=True,
                )
        except (OSError, subprocess.SubprocessError) as exc:
            LOG.warning("screen OCR unavailable: %s", exc)
            return []
        # Screen text is most valuable for names, commands, filenames and
        # product terms. Keeping only distinctive tokens prevents ordinary UI
        # prose from rewriting otherwise-correct speech.
        words = set(WORD_RE.findall(result.stdout))
        return sorted(
            word for word in words
            if len(word) >= 4
            and (not word.islower() or any(ch.isdigit() or ch in "_.+/#-" for ch in word))
        )

    def correct(self, transcript: str) -> str:
        context = self.words()
        if not context:
            return transcript

        def replace(match: re.Match[str]) -> str:
            spoken = match.group(0)
            normalized = spoken.casefold()
            candidates = [
                item for item in context
                if abs(len(item) - len(spoken)) <= 2
                and item.casefold() != normalized
            ]
            if not candidates:
                return spoken
            best = max(
                candidates,
                key=lambda item: SequenceMatcher(None, normalized, item.casefold()).ratio(),
            )
            score = SequenceMatcher(None, normalized, best.casefold()).ratio()
            # This deliberately requires a near-match: OCR is a contextual
            # spelling hint, not a general-purpose rewrite model.
            return best if score >= 0.84 else spoken

        corrected = WORD_RE.sub(replace, transcript)
        if corrected != transcript:
            LOG.info("OCR context corrected %r -> %r", transcript, corrected)
        return corrected


class Parakeet:
    def __init__(
        self, model_dir: Path, threads: int, screen_context: bool,
        screen_context_toggle_file: Path | None = None,
    ) -> None:
        required = {
            "encoder": model_dir / "encoder.int8.onnx",
            "decoder": model_dir / "decoder.int8.onnx",
            "joiner": model_dir / "joiner.int8.onnx",
            "tokens": model_dir / "tokens.txt",
        }
        missing = [str(path) for path in required.values() if not path.is_file()]
        if missing:
            raise FileNotFoundError("Missing model files: " + ", ".join(missing))
        self.recognizer = sherpa_onnx.OfflineRecognizer.from_transducer(
            encoder=str(required["encoder"]),
            decoder=str(required["decoder"]),
            joiner=str(required["joiner"]),
            tokens=str(required["tokens"]),
            num_threads=threads,
            sample_rate=16000,
            feature_dim=128,
            provider="cpu",
            model_type="nemo_transducer",
            decoding_method="greedy_search",
        )
        self.screen_context = ScreenContext(
            screen_context, screen_context_toggle_file
        )

    def transcribe(self, wav_path: Path) -> str:
        with wave.open(str(wav_path), "rb") as wav:
            if wav.getnchannels() != 1 or wav.getsampwidth() != 2:
                raise ValueError("Expected mono 16-bit PCM WAV")
            rate = wav.getframerate()
            samples = np.frombuffer(wav.readframes(wav.getnframes()), dtype="<i2")
        audio = samples.astype(np.float32) / 32768.0
        stream = self.recognizer.create_stream()
        stream.accept_waveform(rate, audio)
        self.recognizer.decode_stream(stream)
        return self.screen_context.correct(stream.result.text.strip())


def create_app(engine: Parakeet) -> FastAPI:
    @asynccontextmanager
    async def lifespan(app: FastAPI):
        app.state.engine = engine
        yield

    app = FastAPI(title="whisrs sherpa-onnx Parakeet sidecar", lifespan=lifespan)

    @app.get("/health")
    async def health() -> dict[str, str]:
        return {"status": "ok", "engine": "sherpa-onnx-parakeet-v3-int8"}

    @app.post("/transcribe")
    async def transcribe(
        file: UploadFile = File(...),
        model: str = Form("parakeet-tdt-0.6b-v3-int8"),
        language: str | None = Form(None),
        hotwords: str | None = Form(None),
        prompt: str | None = Form(None),
    ) -> dict[str, str]:
        del model, language, hotwords, prompt
        data = await file.read()
        if not data:
            raise HTTPException(status_code=400, detail="Empty audio")
        with tempfile.NamedTemporaryFile(suffix=".wav") as tmp:
            tmp.write(data)
            tmp.flush()
            try:
                text = app.state.engine.transcribe(Path(tmp.name))
            except (ValueError, wave.Error) as exc:
                raise HTTPException(status_code=400, detail=str(exc)) from exc
        return {"text": text}

    return app


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model-dir", type=Path, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument(
        "--screen-context", action="store_true",
        help="use local KDE screenshot OCR as a conservative spelling hint",
    )
    parser.add_argument(
        "--screen-context-toggle-file", type=Path,
        help="enable screen OCR only while this file exists",
    )
    args = parser.parse_args()
    import uvicorn

    uvicorn.run(
        create_app(Parakeet(
            args.model_dir.expanduser(), args.threads, args.screen_context,
            args.screen_context_toggle_file.expanduser()
            if args.screen_context_toggle_file else None,
        )),
        host=args.host,
        port=args.port,
    )


if __name__ == "__main__":
    main()

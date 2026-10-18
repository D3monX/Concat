# Optional leading-silence skipping

Independent branch `feat/caption-leading-silence`, based on official main.
No workflow edits, timestamp-library patches, caption padding or word reveal.

The captions dialog offers **Skip leading silence**, off by default and held
for the current editor session. This experimental energy-based option can help
Small when transcription starts with a quiet prefix. It applies to any installed
Whisper model and needs no extra model or service. Existing API requests omit
`skipLeadingSilence` and continue to transcribe the complete window.

After decoding the selected clip's source window to mono 16 kHz floats, the
detector computes 20 ms RMS frames. It finds the first active frame above 8% of
the peak frame RMS with at least four active frames out of five, and retains
250 ms before it. Prefixes shorter than 300 ms are kept. Silence, insufficient
evidence, non-finite input or speech already at the beginning keeps all samples.
Only the prefix is skipped; interior pauses are untouched.

Whisper receives an in-memory slice. Source media, source_start, project data
and timeline positions are never trimmed or rewritten. Segment start and end
receive `skipped_samples / 16000.0` exactly once, before existing timeline speed
conversion. Progress uses the remaining sample duration. The log records the
skip duration. Recognition can change, so this is opt-in rather than a default
fix for all models.

This is an energy heuristic, not a speech detector. Music/noise may count as
activity and quiet words before a louder passage can be missed. The threshold
is supported by one isolated-vocal experiment, not a multilingual evaluation.
On vocal.MP3 it selects 64,160 samples (4.01 s). Existing-binary tests returned
Small phrase starts around 4.01, 12.51, 25.01, 30.01 and 39.01 s after restoring
the source clock. Some Base recognition got worse. Neither result demonstrates
perfect acoustic boundaries.

## Verification before integration

The branch includes native unit tests for retained preroll, empty/silent/short
input, immediate speech, isolated clicks, low gain, invalid input and offset
mapping through clip speed. They have not been compiled or executed: no build
was requested. Static syntax, wiring and whitespace checks plus an independent
diagnostic reproduction of the detector can be performed without a build.
Rust type checking, formatting, Slint compilation, clippy and native tests remain
required before a PR. English/Italian strings are supplied. The locale inventory
tool also reports pre-existing stale voice-style keys and missing translations;
unrelated locale cleanup is intentionally excluded from this branch.

When later merging word reveal, restore this same offset on every token timing
before grouping/normalizing words. Do not add it again in the caption pane.
When combining caption-timing padding, apply that separately to the restored
segment bounds. That integration has not been done here.

# Word-by-word captions

Prepared independently on `feat/caption-word-reveal` from official main.
Not merged into `test/integration`; no build is started for this branch.

The Text inspector has an opt-in **Word by word** switch (Italian:
**Comparsa parola per parola**). A caption remains one text clip on its existing
track. The switch uses the normal inspector commit/undo mechanism. Turning it
off restores the complete phrase without discarding its word timings.

The complete content is shaped and wrapped before visibility is considered.
Hidden glyphs keep their advance, but paint no fill, stroke, shadow or colour
font marks. The complete background plate remains visible. This also keeps
centring, line breaks and vertical placement fixed when later words appear.

`TextStyle.wordReveal` defaults to false for older documents. Optional
`wordStarts` contains normalized onsets in the text renderer's word order.
Times survive save/load and scale with caption duration. Editing content clears
the old timings; creating another caption or saving a reusable style never
copies the previous caption's timings. A wrong timing count falls back as a
whole to even intervals, from the clip start to 80% of its duration, leaving
the final 20% for reading. An untimed one-word title is visible from the start;
a timed one-word title uses its measured onset.

The transcriber now requests token timestamps from the installed Whisper model
with `FullParams::set_token_timestamps(true)`. It preserves ordinary text tokens
as byte spans plus their `t0`/`t1` times (centiseconds converted to seconds).
Special and timestamp tokens are excluded. Bytes are reconstructed and compared
with the complete segment before trimming whitespace: individual tokens may
split a UTF-8 character, and decoding each token as a lossy string would corrupt
the word mapping. A reconstruction failure keeps the text but discards timing.

The caption pane groups these spans with `concat_text::word_ranges`, the same
Unicode line-break segmentation used by the painter. Each word takes the onset
of the token containing its first byte. Subword tokens are not separate words;
a token containing multiple words gives them the same onset rather than
inventing an interpolation. Onsets become caption fractions after correcting
for the source clip's speed and the caption's actual duration, including its
minimum duration. They are saved even when the reveal switch is off.

Missing, invalid, incomplete, backwards or entirely identical multi-word
onsets use the regular cadence
as a fallback, as requested by the user. The text inspector identifies saved
word times versus regular cadence. Script captions also keep the fallback.
Existing captions without word timing need to be transcribed again by the
eventual new build; the existing executable and projects are not changed here.

Whisper calls token timestamps **experimental**. These are acoustic timing
estimates from whisper.cpp, not DTW or the separate forced aligner used in the
earlier audio experiment. Early first words, silence and recognition mistakes
can still affect synchronization. No new model, download, language restriction
or speech dependency is introduced. The API was checked against the published
whisper-rs 0.16.0 crate, with its SHA-256 verified against `Cargo.lock`.

API reference: [Whisper segment tokens](https://docs.rs/whisper-rs/0.16.0/whisper_rs/struct.WhisperSegment.html),
[token timestamps](https://docs.rs/whisper-rs/0.16.0/whisper_rs/struct.FullParams.html#method.set_token_timestamps).

The host caches private prefix PNG states at the requested preview/export
resolution. These are generated render cache entries, never project media,
additional clips or tracks. Both playback/prefetch and export select the same
state on the quantized timeline clock, including backward seeks. Export uses
the still reader pool rather than a decoder frozen on the first state. Only
needed states are decoded. The existing title cache sweep also covers these
PNGs. A cache write failure logs a warning and leaves the caption readable as
ordinary text.

The first render costs more than a static title because it paints each prefix.
Profile long titles, font/size dragging and 4K projects before distribution;
this implementation prioritizes exact native glyph/plate rendering over a
rectangle mask that could leave parts of shadows or outlines visible.

## Degenerate timings and diagnostics

A whole phrase whose words share one onset has no usable reveal progression.
Both newly generated captions and saved projects use regular cadence in that
case. Partial ties remain valid when other words advance, since one Whisper
token can contain more than one word. A single measured word remains valid.

Optional raw token diagnostics require both `CONCAT_WHISPER_TIMING_DEBUG=1`
and debug logging (`CONCAT_LOG=debug`). They record byte reconstruction and
Whisper centisecond onsets before grouping. They can contain transcript text;
ordinary application logs do not include these raw token traces.

## Validation status

Static validation covers Rust syntax, exhaustive struct literals, label wiring,
the pinned whisper-rs API, byte reconstruction, timing fallback and shared
preview/export state selection. It does not replace type checking, Slint
compilation or native tests. Native regression tests are included for stable
layout, Unicode text, token grouping, saved timings, collapsed onsets, switch
undo, one clip/one layer, reverse seeking and prefetch/export selection.

This port preserves upstream's newer backdrop rendering and SRT import/export.
The imported SRT route has no word times and uses the regular fallback if its
text animation is enabled. No workflow, CPU setting, model manifest or vendored
Whisper parser is changed by this branch.

Before integration, run formatting, Slint compilation and focused tests for
`concat-project`, `concat-text`, `concat-host`, `concat-speech`, `concat-export`
and the caption pane in `concat`. The local Rust toolchain is unavailable, so
native compilation and those tests remain pending. Do not claim that earlier
build results validate this port to the current upstream code.

When integration is authorized, test on a copy of the project using the same
`vocal.MP3`, with Base and Small separately: toggle the Text inspector option,
check that saved word times are used, test the long phrase with collapsed
onsets, scrub backwards, export, save/reopen and edit text. Check wrapped text,
caption transforms and fades. Compare preview and export, and profile long
captions and 4K projects.

## Independent contribution branches

This feature neither includes the whisper.cpp timestamp backport nor the
caption reading margins. Integrate and test each separately. When combining
with `feat/caption-timing`, recompute normalized word onsets relative to the
final padded caption start and duration, including clip speed. Margins must not
cause words to appear early or stretch the measured onsets. The regular fallback
remains for captions without usable word times.

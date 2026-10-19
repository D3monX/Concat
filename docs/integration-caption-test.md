# Windows caption integration test

`test/integration` combines the independently prepared branches:

- `fix/whisper-timestamp-gaps`: upstream whisper.cpp gap correction.
- `feat/caption-timing`: 0.75 s reading margins and a 2 s minimum when space
  permits. Avoiding overlap takes priority; clip edges constrain the result.
- `feat/caption-word-reveal`: one caption, stationary word layout, Whisper token
  timings when usable, regular cadence when absent/invalid/collapsed.
- `feat/caption-leading-silence`: optional quiet-prefix skipping with original
  timestamps restored; off by default.

No branch is rewritten. The legacy `fix/windows-build-diagnostics` ref adds no
commits relative to the shared main and introduces no extra change to merge.
The integration workflow still builds Windows x64 on a branch update. Only the
existing ISA scan is advisory; its other build and smoke gates are unchanged.

## How the clocks combine

Whisper transcribes the retained sample slice. The skipped-sample offset is
added to BOTH segment boundaries and token boundaries. The caption pane divides
all those times by clip speed, once. It computes non-overlapping readable
caption bounds and then normalizes each acoustic word onset relative to those
final bounds. Adding lead/trail space must not advance/stretch the measured
onsets. Words beyond a shortened caption's end use regular fallback rather than
an unreachable reveal time. These merge-specific behaviours have regression
tests prepared in captions.rs.

The experimental energy and token-timing limitations still apply. In particular,
the library's suspected energy-coordinate/token-anchoring issue has NOT been
patched here. Collapsed word times use the requested regular fallback.

## Trying the build

Use a copy of the project. Generate captions on vocal.MP3 using Base and Small.
For Small, compare **Skip leading silence** off/on in the captions dialog.
Reading margins apply automatically to newly generated speech captions.
Enable **Word by word** in the Text inspector on the captions to try the reveal.
The option is off by default; old captions need regeneration to acquire word
timings. Check that preview and export agree, existing words stay stationary,
and the long phrase uses regular fallback when timings collapse.

No local application compilation or native tests have been run. Static syntax,
API wiring and detector checks precede the requested GitHub Actions build.
The build's result and interactive/audio quality remain to be verified.
# Typography integration

This integration also includes `feat/text-typography`: AA / aa / Aa and
independent bold, italic, underline and strikethrough controls. Test these in
combination with word reveal, both in preview and video export. Decorations
appear with their words; restoring original case preserves the source text.
Original saved word timings remain usable when the displayed word count agrees,
otherwise the existing regular fallback applies. The inspector checks readiness
against the displayed text as the renderer does.

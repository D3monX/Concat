# Generated caption reading time

Speech captions use the recognised phrase's start and end, converted to
playback seconds using the source clip's speed, then add 0.75 seconds
before and after. A short caption is extended to two seconds where space
allows. Extending a short caption is balanced around the phrase; at a
boundary, the remaining time is taken from the other side.

Captions stay within the source clip. Neighbouring phrases share the
midpoint between the preceding speech end and the following speech start.
Neither caption crosses that boundary, even when recognition ranges overlap.
Avoiding overlaps takes priority over the two-second minimum and the
0.75-second margins. A slot shorter than the title engine's one-frame
minimum cannot hold a separate caption and is omitted rather than enlarged
across the next caption.

Only newly generated speech captions use these rules. Existing titles,
imported SRT cues, manually timed captions and script captions are unchanged.
The rules do not modify Whisper's raw segment timestamps, model files,
language selection or decoding. They do not add word animation or word
timestamps. This feature is independent of the whisper.cpp parser fix.

## Validation before integration

The timing module includes unit cases for full margins, short phrases,
clip boundaries, neighbouring phrases, dense speech and overlapping
recognition ranges. Run them in the normal workspace test environment:

```sh
cd src
cargo test -p concat panes::captions::timing::tests
```

Before merging into `test/integration`, test the timestamp-only build.
After integration, generate captions on a copy of the same project, with
Base and Small. Check the first and last caption, a short phrase, close
phrases and a clip at a speed other than 1x: margins should remain 0.75
playback seconds where space allows. Import an SRT as well and confirm its
times are retained. Keep the original project unchanged.

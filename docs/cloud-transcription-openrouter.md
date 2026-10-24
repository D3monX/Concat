# OpenRouter transcription

Recovered independently from cloud bundle commit `4fb5b257`, on upstream
`971ad1d9`. Personal integration is not imported or overwritten by this branch.

## Interface

In Captions, enable voice transcription and select Provider and Model:

| Provider | Model |
| --- | --- |
| Local (Whisper), the default | Installed Whisper models |
| OpenRouter | MAI Transcribe 2 |

The cloud model is displayed separately and is read-only while it is the only
supported choice. Adding models requires checking their transcription format
and measured-word timing support, not just accepting an arbitrary model ID.
Microsoft supplies the model; the credential belongs to OpenRouter.
There is no automatic cloud fallback or paid retry.

Enable cloud transcription in Settings > Transcription. That page has the
masked API key field, Save / replace, Delete key and Read again. Existing keys
remain in their current secure/fallback store; no reentry is needed. New cloud
enablement defaults to off for older preferences. The stored key is never
populated back into the field. The draft clears after save, close, leaving the
page or disabling cloud. The captions sheet only selects the provider/model
and asks for consent: "I agree to send this audio to OpenRouter."

The last chosen provider is persisted independently of the installed Whisper
model. Reopening captions restores that provider when cloud is enabled. The
dropdown indices are synchronized with each new sheet data snapshot, so a
Select's mutable current index cannot retain a stale provider while Rust has
reset to local. The model row is selected from the effective provider.

Each run consumes consent to a paid audio transfer, tied to the chosen clip
snapshot. Changing source/provider, saving/replacing credentials, or closing
the sheet invalidates it. The start button checks that the clip still matches.
A worker handles storage access and OS prompts; UI publishing never reads a key.

## Credentials on each platform

A shared `concat_speech::credentials` service reads, saves, replaces, deletes,
and reports sanitized status. The secret has redacted Debug formatting and
zeroized Rust buffers; this does not guarantee erasure of OS, Java, UI or HTTP
library copies. It never enters project serialization, logs or API/MCP tools.
Environment variables are no longer needed or read for cloud credentials.

| Platform | Protected storage |
| --- | --- |
| Windows | Credential Manager through keyring's Windows native adapter |
| macOS / iOS | Keychain through keyring's Apple native adapter |
| Linux | Secret Service, using its pure Rust asynchronous transport |
| Android | Private SharedPreferences ciphertext, AES/GCM key in Android Keystore |

Android initializes its adapter from the existing activity before the editor
starts. It uses the existing Java/JNI bridge. Only fixed status codes cross
back on failures; no Java exception detail containing a key is reported.

When protected storage is unavailable, Save uses the agreed plaintext file
fallback: `credentials/openrouter.json` under Concat's private data/config
folder. AppDirs selects `portable/` beside the executable for portable builds.
Installed packages and mobile apps use their writable private folder rather
than modifying the executable/package directory. Windows uses non-roaming local
app data; iOS uses durable Application Support rather than purgeable Caches.
The UI explicitly distinguishes protected storage from a local plaintext file.
Unix directories/files have permissions 0700/0600; Windows files inherit their
parent folder's ACL. A shared portable folder is not an encrypted vault.

Routing metadata is written atomically using a private temporary file, flushed,
and replaced. It selects either native storage, the local key, or a deletion
tombstone. This prevents a stale OS key from resurfacing after an offline
fallback replacement/deletion. A successful native save replaces the fallback
file with metadata containing no secret. Delete blocks reuse before trying OS
cleanup; if the store is locked/offline the UI reports cleanup pending, and
Delete can be retried. A locked store or explicit denial does not trigger a
plaintext replacement. Corrupt files fail rather than falling through to an
old key. Portable folder backups include any plaintext fallback key.

## Transport and captions

Only the selected source window/audio stream is decoded to an in-memory
16 kHz mono PCM16 WAV and sent over HTTPS. The final decoder packet is truncated
at the selected source out-point. No temporary audio file is written.
Limits: 300 seconds, 10 MiB before base64, 90 seconds per operation, 4 MiB response.
Cancelling drops the request future and prevents caption insertion; it cannot
recall audio already sent or charges already incurred. The worker retains its
slot until finished, including cancellation before worker scheduling.

The endpoint receives `verbose_json` and word timestamp granularity. The parser
requires measured word times, with finite positive intervals and ordered
onsets. Duration reported by the provider is informational; decoded audio
defines the valid clock. Overlapping words are accepted without moving their
onsets. Up to 50 ms of drift at the source boundaries is clipped to actual
audio bounds; larger excursions still fail. A phrase ends at the latest word
end in its group, including overlaps. It never synthesizes missing times.
When the full transcript and timed words differ, every timed word is retained
and phrase text is reconstructed from that stream. Matching full text keeps
its original punctuation/spacing. It groups words into phrases and retains
UTF-8 spans and measurements in `Segment.tokens`; the upstream-only Whisper
path leaves that field empty. Existing requests which omit backend/consent
still select local Whisper. The existing personal integration's token/VAD/
silence-offset and word-reveal implementations must be preserved at merge.

Contract: https://openrouter.ai/docs/guides/overview/multimodal/stt
The user reported one successful real MAI transcription and timestamp errors
on another clip with the previous parser. The revised parser still needs
real-provider verification on those clips; synthetic tests are not evidence
of recognition quality or billing. No paid calls are made by development checks.

## Timestamp diagnostics

Each request logs the fixed endpoint's model, `response_format=verbose_json`,
word granularity, requested/submitted seconds, PCM sample count and payload byte
count. A response logs HTTP status, a constrained generation ID when supplied,
and only the presence/count of expected fields and numeric duration. It never
logs the response body, transcription text, raw audio or API key.

Validation failures have a distinct code and safe numeric details shown in
the error and log. Codes distinguish missing/wrong-type fields, JSON syntax
(category/line/column only), invalid word intervals, backwards onsets and word
times beyond audio tolerance. Word indices are one-based. Metadata duration
discrepancies, boundary adjustments and use of timed-word text are recorded as
warnings instead of discarding a usable transcript. No arbitrary interpolation
or local/remote retry is introduced.
An HTTP success followed by one of these failures is a Concat parser rejection.

The loopback HTTP test checks that the actual transmitted JSON includes both
`response_format: "verbose_json"` and `timestamp_granularities: ["word"]`.
Targeted tests check diagnostic categories and that arbitrary remote values are
never reflected into errors. A preference regression test covers cloud opt-in,
last provider and the independent local model choice.

## Verification limits

Targeted tests cover parsing/HTTP errors/cancellation, legacy dispatcher
behavior, pre-scheduling cancellation, credential fallback, replacement,
migration back to protected storage, deletion while offline, locked stores,
corrupt records, and redacted diagnostics. Credential tests use an injected
fake store and temporary directories, never the developer's real credentials.

No compilation, Clippy, test suite or dependency source installation is run
locally in this task. Formatting, whitespace, locale/model inventories and
manifest/lock dependency inspection are the local checks. CI is explicitly
requested on this feature branch; it does not imply native credential or
interactive verification on every platform. Desktop/mobile device checks,
real-provider testing and integration/build remain pending.

# OpenRouter transcription

## Personal integration

The personal integration combines this feature with the current caption reading
margins, typography, word reveal and background title preparation. Cloud word
times flow through the existing speed/margin normalization. Renderer word ranges
with leading punctuation can match overlapping measured word spans without
inventing onsets. The optional leading-silence switch also applies to cloud
requests: it trims only the decoded in-memory input, then restores that offset
once on both phrase and word times. The local Whisper/VAD/token-clock path and
the current title/audio/export implementations are retained.

Targeted integration tests cover punctuation matching and cloud offset
restoration. They are prepared, not executed locally. A successful feature CI
validates the feature commit, not the later integration merge.

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

The same captions sheet has a masked API key field, Save / replace, Delete key,
and Read again. The stored key is never populated back into that field. The
input is cleared after save, close, provider switch, or transcription start.
The sheet scrolls to keep the controls reachable on short/mobile windows.

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
requires transcript, duration and complete ordered measured word times. It
never synthesizes missing times. It groups words into phrases and retains
UTF-8 spans and measurements in `Segment.tokens`; the upstream-only Whisper
path leaves that field empty. Existing requests which omit backend/consent
still select local Whisper. The existing personal integration's token/VAD/
silence-offset and word-reveal implementations must be preserved at merge.

Contract: https://openrouter.ai/docs/guides/overview/multimodal/stt
Real MAI responses remain untested: duration tolerance (150 ms), word overlap,
exact word/text correspondence, recognition quality and billing need provider
trials. No paid network calls are performed by the development checks.

## Verification

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

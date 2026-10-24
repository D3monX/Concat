# Optional OpenRouter transcription

This branch recovers only the cloud transcription feature from the local
`Concat-cloud-transcription.bundle`, archived at commit `4fb5b257`. It starts
from upstream `971ad1d9`; it does not import the bundle's older personal
integration, typography, audio merge, reading margins, silence skipping or
word reveal implementations. Their current branches remain independent.

## Captions interface

When transcribing a clip's voice, the captions sheet offers two rows:

| Provider | Model |
| --- | --- |
| Local (Whisper), the default | Installed Whisper models |
| OpenRouter | MAI Transcribe 2 (the single supported cloud model) |

The cloud model row is read-only until other compatible models are implemented.
The provider identifies the service holding the API credential; the model
identifies the recognizer. Microsoft is the model vendor, not a second
credential provider in this interface. Switching providers keeps the local
Whisper model choice. There is no automatic cloud fallback or retry.

Each cloud run requires acknowledgement of the paid audio transfer. Consent
belongs to the selected clip snapshot and is consumed when starting. Changing
the source or provider, closing the sheet, or cancelling clears it. The start
button also checks whether the current clip still matches that snapshot.
Cancellation suppresses caption insertion; audio already sent and provider
charges cannot be recalled.

Only the selected audio stream and source window are decoded into an in-memory
16 kHz mono PCM16 WAV and sent over HTTPS. A final decoder packet is truncated
at the source window's out-point. Limits are 300 seconds, 10 MiB before base64,
90 seconds per network operation, and 4 MiB per response. No audio temporary
file is created. Errors do not include provider response bodies or transcripts.

The response must include text, duration, and measured word timestamps.
Invalid or missing times are rejected rather than synthesized. Words are
grouped into caption phrases, and their UTF-8 spans and measured times remain
in `Segment.tokens`. On the upstream-only branch local Whisper leaves that
field empty, preserving its existing transcription behavior. Existing clients
which omit the new request fields still select Whisper without cloud consent.

Contract: [OpenRouter speech-to-text documentation](https://openrouter.ai/docs/guides/overview/multimodal/stt).
Real MAI responses remain untested: in particular duration matching (150 ms),
nonoverlapping word times, and exact transcript/word correspondence need a
provider trial. No paid calls are part of development checks.

## Credentials in the recovered implementation

The recovered transport reads `OPENROUTER_API_KEY` from the process environment.
This is a temporary developer path, not the final cross-platform credentials UI.
Concat writes no credential into projects, preferences, logs or `.env` files.
There is currently no in-app key entry or persistent credential store. The
environment path alone is unsuitable as a normal mobile setup flow.

## Cross-platform credential design (not implemented yet)

Use one application credential service (`save`, `read`, `delete`, `status`),
with native storage selected explicitly for each platform:

| Platform | Persistent storage |
| --- | --- |
| Windows | Windows Credential Manager |
| macOS | Keychain, choosing the appropriate signed/sandboxed app access |
| iOS | Keychain protected data with app access groups |
| Linux desktop | Secret Service through the user's session keyring |
| Android | App-private encrypted data, encryption key in Android Keystore |

Android Keystore stores cryptographic keys: it is used to protect the API
credential, not as a plain password preferences file. An Android adapter must
be initialized with the activity/application context; Apple access groups and
packaging must match the app's signing setup. Distinguish an unavailable store
from a temporarily locked store or an explicit denial of access. A locked
store must not cause an existing saved key to be overwritten.

The same Settings > Transcription UI can offer a masked key field, Save,
Replace and Delete, and a status such as Configured / Locked / Not configured.
Never read the stored secret back into the UI to display it; keep only its
status there. A key belongs to a provider, independent of the chosen model.
The UI passes an explicitly entered secret to the credential service and
clears the input afterwards. HTTP authorization is created only when starting
a run. Credentials must stay out of project serialization and API/MCP tools.

The user's chosen fallback is a local credential file when secure persistent
storage is unavailable. This is plaintext storage, not an encrypted vault;
the UI must accurately show "Saved in local file" rather than "Saved securely".
Keep the file separate from project files, preferences, logs and exports.
Restrict its permissions to the app/user where the platform supports it.
Replacing/deleting a credential must handle its active storage location without
leaving stale fallback copies behind.

For a portable desktop install the user wants the file in the software folder,
provided it is writable and outside any signed app bundle. Installed apps
(such as Windows under Program Files) and mobile apps need the app's private
data directory instead: the executable/package directory may be read-only.
Use the existing AppDirs conventions on each platform; do not request elevated
privileges just to save a credential. Portable folder backups will include a
plaintext credential file unless that file is explicitly excluded.

Session-only use can remain an option when no writable location is available
or the user does not want to remember a key. Stored secrets remain local to
each device; synchronizing project files does not copy them. No file fallback
or credential UI is implemented by this recovery commit.

`keyring-core` plus explicitly chosen native adapters is a candidate, not a
dependency added by this recovery. Its mock/sample stores are not production
secure stores. Android initialization, Linux keyring availability and Apple
signing need platform checks before choosing versions and implementing it.

References:
- [Common credential API](https://docs.rs/keyring-core/latest/keyring_core/)
- [Android native adapter](https://docs.rs/android-native-keyring-store/latest/android_native_keyring_store/)
- [Apple native adapters](https://docs.rs/apple-native-keyring-store/latest/apple_native_keyring_store/)
- [Android Keystore](https://developer.android.com/privacy-and-security/keystore)

Future providers/models should be registered with stable IDs, credential
provider, supported request/response formats, and timestamp capabilities. Only
compatible transcription models belong in the selector; an arbitrary model ID
does not guarantee the measured timing required by captions. Changing the
provider/model should invalidate any pending paid-transfer consent.

## Verification limits

The recovered synthetic-fixture and loopback HTTP tests cover parsing,
request construction, cancellation, limits, and error sanitization. Dispatcher
tests cover legacy defaults and rejection before paid I/O; a SingleFlight test
covers cancellation before worker scheduling. They have not been run on this
recovered branch. Earlier bundle test claims do not validate the adapted code.
No compilation, suite, paid call, push or Actions run is authorized in this step.
Only whitespace, locale/model inventories, dependency inventory and targeted
source/diff checks are performed locally. Native desktop/mobile builds and
interactive checks remain pending.

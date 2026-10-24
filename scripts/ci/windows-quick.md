# Windows Quick (personal fork)

`windows-quick.yml` builds **test/integration** by default, independently of
`build-app.yml`. The latter remains the shipping build, with installers and the
full compatibility checks. Quick uses the existing Cargo `quick` profile: no
LTO, incremental compilation, unoptimised editor UI, optimised engine crates.
Native dependency pins, baseline CPU flags and dynamically selected whisper
CPU modules are unchanged.

The workflow lives on `feat/windows-quick-build`. Its first push starts the
benchmark without publishing integration again and starting another full build.
Until the workflow is also present on the repository's default branch, GitHub
does not offer manual dispatch. To build the current integration again, open
the Windows Quick run in Actions and choose **Re-run all jobs**. Each attempt
checks out the latest remote `test/integration`; consult `build-info.json` and
the job summary for the actual source SHA, rather than the workflow commit.
When manual dispatch is available, its `ref` input can also pin an exact SHA.

Download the `Windows-Quick-portable-<source SHA>` artifact and extract its
portable ZIP into a separate writable folder. Run `concat.exe` there. Keep the
`portable/` directory beside it: Concat already supports that marker and stores
its application data there. No existing installation, user credentials, media
or models are copied into the artifact. OS-protected API credentials remain
specific to the user/device; this workflow does not change credential storage.

Two separate caches preserve native SDKs and the entire Cargo target tree,
including workspace crates and incremental data. Native libraries are cached
outside `target` too, because cached build scripts refer to those paths.
Workspace compatibility includes compiler, runner image, MSVC/SDK, CMake,
native flags, manifests, lockfiles, Cargo configuration and workflow/helper
implementation. Compatible previous snapshots are restored; successful runs
save a new immutable snapshot. GitHub cache eviction or a compatibility change
can cause a cold build. Unchanged tracked inputs get their previous timestamps
back only after their SHA-256 matches; changed inputs keep their new timestamps
and Cargo still performs its normal fingerprint checks.

The staged bundle passes the existing Windows DLL inventory, `--version` and
GUI `--smoke` checks before ZIP/upload. Quick does not run the shipping
installer, instruction scan or emulated CPU checks. The ZIP contains source
and workflow SHAs, profile, build link, cache status and compile duration;
its SHA-256 is uploaded alongside it. Artifacts expire after seven days.

The summary records compile and total job elapsed time through cache save and
upload; individual step times separate SDK setup, cache transfer, staging and
packaging. Runner queue time is excluded. **10–15 minutes is a warm rebuild
goal**, not a measured guarantee: the first run compiles dependencies and seeds
both caches. Compare a later small-edit run before judging the warm target.

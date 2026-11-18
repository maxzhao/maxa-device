# Corresponding source for this companion APK

Get `maxa-device-companion-<version>-source.tar.gz` from the same distributor as
this APK. `assets/embedded-api-source.json` in the APK pins the archive SHA-256;
`SHA256SUMS` beside the artifacts identifies the exact APK and source archive.
The Windows installer carries the same APK, source archive and checksums.

On Linux x86_64 with JDK 17, Python 3, Bash and Rust/rustup, extract the archive,
enter `maxa-device-companion-source`, run `bash android/agent/build.sh`, then
`bash android/build-companion.sh`. They provision the pinned NDK, SDK and Gradle,
build the headless payload and APK, and generate a fresh local signing key if
none exists. No original signing material is needed for rebuilding and
installing on a test device. The original signature is not reproduced.

`src-tauri/Cargo.lock` fixes the Rust dependency source packages/checksums;
`cargo vendor --locked --manifest-path src-tauri/Cargo.toml` retrieves the exact
packages from their recorded registries. The vendored adbutils-rs source is in
this archive. `android/companion/app/gradle.lockfile` and
`android/companion/gradle/verification-metadata.xml` pin Gradle package versions
and binary inputs. The APK runtime inputs are the Kotlin standard library and
JetBrains annotations: retrieve their matching `-sources.jar` files from Maven
Central at the group/artifact/version in the lockfile. These locations are for
retrieving the exact sources, not permission to substitute a different version.
When redistributing the APK, also keep the corresponding dependency sources
available from the same place as the binary; do not rely solely on external
registry availability. The archive does not itself snapshot those registries.

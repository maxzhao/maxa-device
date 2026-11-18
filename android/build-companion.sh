#!/usr/bin/env bash
# Linux maintainer build; all SDK, Gradle and signing state is private to CACHE.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cache=${MAXA_ANDROID_CACHE:-$HOME/.cache/maxa-device-android}
proxy=${HTTPS_PROXY:-${HTTP_PROXY:-}}
for tool in curl unzip sha256sum keytool awk python3; do command -v "$tool" >/dev/null || { echo "Missing $tool" >&2; exit 2; }; done
mkdir -p "$cache/downloads" "$cache/sdk/platforms" "$cache/sdk/build-tools" "$cache/signing" "$cache/gradle-home" "$cache/android-user"
chmod 700 "$cache/signing"
curl_args=(); jvm_proxy=()
if [[ -n "$proxy" ]]; then
    [[ "$proxy" =~ ^http://([A-Za-z0-9.-]+):([0-9]+)$ ]] || { echo 'Use an unauthenticated http://host:port proxy' >&2; exit 2; }
    curl_args=(--proxy "$proxy")
    jvm_proxy=(-Dhttp.proxyHost="${BASH_REMATCH[1]}" -Dhttp.proxyPort="${BASH_REMATCH[2]}" -Dhttps.proxyHost="${BASH_REMATCH[1]}" -Dhttps.proxyPort="${BASH_REMATCH[2]}" -Dhttp.nonProxyHosts='localhost|127.*')
fi
fetch() {
    local name=$1 url=$2 sha=$3
    if [[ ! -f "$cache/downloads/$name" ]]; then
        curl --fail --location --retry 2 --max-time 600 "${curl_args[@]}" "$url" -o "$cache/downloads/$name.part"
        mv "$cache/downloads/$name.part" "$cache/downloads/$name"
    fi
    echo "$sha  $cache/downloads/$name" | sha256sum -c -
}
fetch gradle-8.9-bin.zip https://services.gradle.org/distributions/gradle-8.9-bin.zip d725d707bfabd4dfdc958c624003b3c80accc03f7037b5122c4b1d0ef15cecab
fetch platform-35_r02.zip https://dl.google.com/android/repository/platform-35_r02.zip 0988cacad01b38a18a47bac14a0695f246bc76c1b06c0eeb8eb0dc825ab0c8e0
fetch build-tools_r35_linux.zip https://dl.google.com/android/repository/build-tools_r35_linux.zip bd3a4966912eb8b30ed0d00b0cda6b6543b949d5ffe00bea54c04c81e1561d88
[[ -x "$cache/gradle-8.9/bin/gradle" ]] || unzip -qo "$cache/downloads/gradle-8.9-bin.zip" -d "$cache"
if [[ ! -f "$cache/sdk/platforms/android-35/android.jar" ]]; then
    unzip -qo "$cache/downloads/platform-35_r02.zip" -d "$cache/sdk/platforms"
    [[ -d "$cache/sdk/platforms/android-35" ]] || mv "$cache/sdk/platforms/android-15" "$cache/sdk/platforms/android-35"
fi
if [[ ! -x "$cache/sdk/build-tools/35.0.0/aapt2" ]]; then
    unzip -qo "$cache/downloads/build-tools_r35_linux.zip" -d "$cache/sdk/build-tools"
    mv "$cache/sdk/build-tools/android-15" "$cache/sdk/build-tools/35.0.0"
fi
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64
export ANDROID_HOME="$cache/sdk" ANDROID_SDK_ROOT="$cache/sdk" ANDROID_USER_HOME="$cache/android-user" GRADLE_USER_HOME="$cache/gradle-home"
unset JAVA_TOOL_OPTIONS _JAVA_OPTIONS JDK_JAVA_OPTIONS GRADLE_OPTS
# Never expose the password in argv, source control or outputs.
if [[ ! -f "$cache/signing/password" ]]; then (umask 077; head -c 32 /dev/urandom | base64 > "$cache/signing/password"); fi
export MAXA_COMPANION_STORE_PASSWORD; MAXA_COMPANION_STORE_PASSWORD=$(cat "$cache/signing/password")
if [[ ! -f "$cache/signing/companion.p12" ]]; then
    (umask 077; keytool -genkeypair -keystore "$cache/signing/companion.p12" -storetype PKCS12 -storepass:env MAXA_COMPANION_STORE_PASSWORD -keypass:env MAXA_COMPANION_STORE_PASSWORD -alias companion -keyalg RSA -keysize 3072 -validity 10000 -dname 'CN=Maxa Device Companion Local Build' -noprompt)
fi
version=$(awk '/^version = / { gsub(/"/, "", $3); print $3; exit }' "$root/src-tauri/Cargo.toml")
IFS=. read -r major minor patch <<< "$version"
version_code=$((10#$major * 1000000 + 10#$minor * 1000 + 10#$patch))
args=(--no-daemon --max-workers=2 -Dorg.gradle.jvmargs='-Xmx2048m -Dfile.encoding=UTF-8' -Dkotlin.compiler.execution.strategy=in-process "${jvm_proxy[@]}" -Pandroid.builder.sdkDownload=false -PcompanionVersion="$version" -PcompanionVersionCode="$version_code" -PsigningStore="$cache/signing/companion.p12")
out_dir="$root/android/dist/companion"
mkdir -p "$out_dir"
stage=$(mktemp -d "$out_dir/.build.XXXXXX")
trap 'rm -r -- "$stage"' EXIT
apk="maxa-device-companion-$version.apk"
source="maxa-device-companion-$version-source.tar.gz"
python3 "$root/android/archive-companion-source.py" "$stage/$source"
source_sha=$(sha256sum "$stage/$source" | cut -d' ' -f1)
args+=(-PcompanionSourceSha256="$source_sha")
cd "$root/android/companion"
# First-time dependency trust must be explicitly generated and reviewed by a maintainer.
# Subsequent builds enforce the committed metadata and dependency lockfiles.
if [[ ${1:-} == --write-verification ]]; then
    "$cache/gradle-8.9/bin/gradle" "${args[@]}" --write-verification-metadata sha256 --write-locks :app:testReleaseUnitTest :app:assembleRelease
else
    "$cache/gradle-8.9/bin/gradle" "${args[@]}" --dependency-verification strict :app:testReleaseUnitTest :app:assembleRelease
fi
# Refuse to publish a mismatched local pair if source changed while building.
python3 "$root/android/archive-companion-source.py" "$stage/check.tar.gz"
[[ $(sha256sum "$stage/check.tar.gz" | cut -d' ' -f1) == "$source_sha" ]] || {
    echo 'Corresponding source changed during build; rerun after changes settle' >&2; exit 1;
}
out="$stage/$apk"
cp app/build/outputs/apk/release/app-release.apk "$out"
"$cache/sdk/build-tools/35.0.0/apksigner" verify --verbose "$out"
"$cache/sdk/build-tools/35.0.0/aapt2" dump badging "$out"
"$cache/sdk/build-tools/35.0.0/aapt2" dump xmltree "$out" --file AndroidManifest.xml
(cd "$stage" && sha256sum "$apk" "$source" > SHA256SUMS)
mv -- "$out" "$stage/$source" "$stage/SHA256SUMS" "$out_dir/"
(cd "$out_dir" && cat SHA256SUMS)

#!/usr/bin/env bash
# Linux x86_64 maintainer build. All downloads, SDK/JDK state and Gradle state stay in CACHE.
# Usage: bash build.sh CACHE NEW_WORK OUTPUT_JAR [HTTP_PROXY_URL]
# Static check only: bash build.sh --verify DEXDUMP JAR TEMP_ROOT
set -euo pipefail
verify_jar() {
    local dexdump=$1 artifact=$2 temp_root=$3 dex_dir evidence name
    dex_dir=$(mktemp -d "$temp_root/u2-dex.XXXXXX")
    evidence="$dex_dir/evidence.txt"
    mapfile -t dex_names < <(unzip -Z1 "$artifact" | grep -E '^classes[0-9]*[.]dex$')
    (( ${#dex_names[@]} > 0 )) || { echo 'No DEX files in helper artifact' >&2; return 1; }
    for name in "${dex_names[@]}"; do
        unzip -p "$artifact" "$name" > "$dex_dir/$name"
        # DEX strings are modified UTF-8. Inspect ASCII instructions as bytes, not decoded Unicode.
        "$dexdump" -d "$dex_dir/$name" | LC_ALL=C awk '
            index($0,"com.wetest.uia2.stub.AutomatorHttpServer.<init>:(I)V") {section="C"}
            index($0,"com.wetest.uia2.Main.main:([Ljava/lang/String;)V") {section="M"}
            /catches       :/ {section=""}
            section!="" {print section " " $0}
        '
    done > "$evidence"
    LC_ALL=C awk '
        /^C / && /const-string/ && /"127[.]0[.]0[.]1"/ {literal++}
        /^C / && index($0,"NanoHTTPD;.<init>:(Ljava/lang/String;I)V") {address=1}
        /^C / && index($0,"NanoHTTPD;.<init>:(I)V") {wildcard=1}
        /^M / && index($0,"Main;.runServer:(I)V") {main=1}
        /^M / && /const-string/ && /"p"/ {port=1}
        END {
            if(literal!=1 || !address || wildcard || !main || !port) {
                print "DEX loopback constructor / Main -p verification failed" > "/dev/stderr"; exit 1
            }
            print "STATIC_BIND=127.0.0.1; MAIN_CLASS=com.wetest.uia2.Main; PORT_ARGUMENT=-p"
        }
    ' "$evidence"
    LC_ALL=C awk '/^C / && (/const-string/ || index($0,"NanoHTTPD;.<init>")) {print substr($0,3)}' "$evidence"
    rm -r -- "$dex_dir"
}
if [[ ${1:-} == --verify ]]; then
    (( $# == 4 )) || { echo 'usage: build.sh --verify DEXDUMP JAR TEMP_ROOT' >&2; exit 2; }
    verify_jar "$2" "$3" "$4"
    exit
fi
if (( $# < 3 || $# > 4 )); then echo 'usage: build.sh CACHE NEW_WORK OUTPUT_JAR [HTTP_PROXY_URL]' >&2; exit 2; fi
for tool in jq curl tar unzip sha256sum patch awk realpath; do
    command -v "$tool" >/dev/null || { echo "Missing maintainer tool: $tool" >&2; exit 2; }
done
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cache=$(realpath -m -- "$1"); work=$(realpath -m -- "$2"); output=$(realpath -m -- "$3"); proxy=${4:-}
[[ $(uname -sm) == 'Linux x86_64' ]] || { echo 'Linux x86_64 required' >&2; exit 2; }
[[ ! -e "$work" && ! -e "$output" ]] || { echo 'NEW_WORK and OUTPUT_JAR must not exist' >&2; exit 2; }
curl_proxy=(); jvm_proxy=()
if [[ -n "$proxy" ]]; then
    if [[ ! "$proxy" =~ ^http://([A-Za-z0-9.-]+|\[[0-9A-Fa-f:]+\]):([0-9]{1,5})$ ]]; then
        echo 'unauthenticated http://host:port proxy required' >&2; exit 2
    fi
    host=${BASH_REMATCH[1]}; port=${BASH_REMATCH[2]}
    (( 10#$port > 0 && 10#$port <= 65535 )) || { echo 'invalid proxy port' >&2; exit 2; }
    host=${host#[}; host=${host%]}
    curl_proxy=(--proxy "$proxy")
    jvm_proxy=(-Dhttp.proxyHost="$host" -Dhttp.proxyPort="$port" -Dhttps.proxyHost="$host" -Dhttps.proxyPort="$port")
fi
mkdir -p "$cache/downloads" "$cache/home" "$cache/android-user" "$work" "$(dirname -- "$output")"
# Do not read global Java/Gradle/Android configuration or inherit injected JVM options.
unset JAVA_TOOL_OPTIONS _JAVA_OPTIONS JDK_JAVA_OPTIONS GRADLE_OPTS ANDROID_SDK_HOME
export HOME="$cache/home" GRADLE_USER_HOME="$cache/gradle-home" ANDROID_USER_HOME="$cache/android-user"
export JAVA_HOME="$cache/jdk" ANDROID_HOME="$cache/sdk" ANDROID_SDK_ROOT="$cache/sdk"
export TZ=UTC LC_ALL=C.UTF-8
downloads=$(jq -er '.downloads | to_entries[] | [.key,.value.url,.value.sha256] | @tsv' "$here/build-inputs.json")
while IFS=$'\t' read -r name url expected; do
    file="$cache/downloads/$name"
    if [[ ! -f "$file" ]]; then
        curl --fail --location --max-time 300 "${curl_proxy[@]}" "$url" -o "$file.part"
        mv -- "$file.part" "$file"
    fi
    [[ $(sha256sum "$file" | cut -d ' ' -f1) == "$expected" ]] || { echo "SHA256 mismatch: $name" >&2; exit 1; }
done <<< "$downloads"
if [[ ! -x "$JAVA_HOME/bin/java" ]]; then mkdir -p "$JAVA_HOME"; tar -xzf "$cache/downloads/jdk.tar.gz" --strip-components=1 -C "$JAVA_HOME"; fi
if [[ ! -x "$cache/gradle-7.3.3/bin/gradle" ]]; then unzip -q "$cache/downloads/gradle.zip" -d "$cache"; fi
if [[ ! -f "$ANDROID_HOME/platforms/android-34/android.jar" ]]; then
    mkdir -p "$ANDROID_HOME/platforms"; unzip -q "$cache/downloads/platform.zip" -d "$ANDROID_HOME/platforms"
    if [[ -d "$ANDROID_HOME/platforms/android-14" ]]; then mv "$ANDROID_HOME/platforms/android-14" "$ANDROID_HOME/platforms/android-34"; fi
fi
if [[ ! -x "$ANDROID_HOME/build-tools/30.0.3/dexdump" ]]; then
    mkdir -p "$ANDROID_HOME/build-tools"; unzip -q "$cache/downloads/build-tools.zip" -d "$ANDROID_HOME/build-tools"
    mv "$ANDROID_HOME/build-tools/android-11" "$ANDROID_HOME/build-tools/30.0.3"
fi
tar -xzf "$cache/downloads/source.tar.gz" --strip-components=1 -C "$work"
(cd "$work" && patch --batch --fuzz=0 -p1 < "$here/loopback-only.patch" && patch --batch --fuzz=0 -p1 < "$here/package-catalog.patch" && patch --batch --fuzz=0 -p1 < "$here/clipboard-safety.patch")
printf 'sdk.dir=%s\n' "$ANDROID_HOME" > "$work/local.properties"
# Enforce all resolved plugin/Maven artifacts and metadata; do not regenerate trust.
cp "$here/verification-metadata.xml" "$work/gradle/verification-metadata.xml"
jvm=(-Duser.home="$HOME" -Dorg.gradle.java.home="$JAVA_HOME" -Dfile.encoding=UTF-8)
(cd "$work" && "$cache/gradle-7.3.3/bin/gradle" --no-daemon --max-workers=2 "${jvm[@]}" "${jvm_proxy[@]}" -Pandroid.builder.sdkDownload=false --dependency-verification strict :app:assembleDebug)
cp "$work/app/build/outputs/apk/debug/app-debug.apk" "$output"
verify_jar "$ANDROID_HOME/build-tools/30.0.3/dexdump" "$output" "$work"
actual=$(sha256sum "$output" | cut -d ' ' -f1)
expected=$(jq -er '.artifact.sha256' "$here/MAXA_UPSTREAM.json")
[[ "$actual" == "$expected" ]] || { echo 'Non-reproducible artifact; inspect inputs, do not silently update the pinned hash' >&2; exit 1; }
printf '%s  %s\nPINNED_ARTIFACT_SHA256=PASS\n' "$actual" "$output"

#!/usr/bin/env bash
# Linux x86_64 build; only ~/.cache/maxa-device-android owns downloaded toolchains.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cache=${MAXA_ANDROID_CACHE:-$HOME/.cache/maxa-device-android}
ndk="$cache/android-ndk-r28c"
archive="$cache/android-ndk-r28c-linux.zip"
ndk_sha256=dfb20d396df28ca02a8c708314b814a4d961dc9074f9a161932746f815aa552f
[[ $(uname -sm) == 'Linux x86_64' ]] || { echo 'Linux x86_64 build host required' >&2; exit 1; }
mkdir -p "$cache"
if [[ ! -d "$ndk" ]]; then
    if [[ ! -f "$archive" ]]; then
        curl -fL --retry 2 https://dl.google.com/android/repository/android-ndk-r28c-linux.zip -o "$archive.partial"
        echo "$ndk_sha256  $archive.partial" | sha256sum -c --quiet
        mv "$archive.partial" "$archive"
    fi
    echo "$ndk_sha256  $archive" | sha256sum -c --quiet
    unzip -q "$archive" -d "$cache"
fi
bin="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"
[[ -x "$bin/aarch64-linux-android30-clang" ]] || { echo 'NDK clang missing' >&2; exit 1; }
if ! rustup target list --installed | grep -qx aarch64-linux-android; then rustup target add aarch64-linux-android; fi
export CC_aarch64_linux_android="$bin/aarch64-linux-android30-clang"
export CXX_aarch64_linux_android="$bin/aarch64-linux-android30-clang++"
export AR_aarch64_linux_android="$bin/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CC_aarch64_linux_android"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$root=/build/maxa-device --remap-path-prefix=$HOME=/build/user"
# Target directory is explicit, not inherited from a maintainer's global config.
export CARGO_TARGET_DIR="$root/src-tauri/target"
cargo build --manifest-path "$root/src-tauri/Cargo.toml" --locked --release --no-default-features --target aarch64-linux-android --bin maxa-device
version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$root/src-tauri/Cargo.toml" | head -1)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'Invalid package version' >&2; exit 1; }
out="$root/android/dist/agent"
mkdir -p "$out"
stage=$(mktemp -d "$out/.package.XXXXXX")
trap 'rm -r -- "$stage"' EXIT
cp "$CARGO_TARGET_DIR/aarch64-linux-android/release/maxa-device" "$stage/maxa-device"
"$bin/llvm-strip" "$stage/maxa-device"
chmod 755 "$stage/maxa-device"
agent="maxa-device-agent-$version-aarch64-linux-android.tar.gz"
tar --sort=name --mtime='UTC 2020-01-01' --owner=0 --group=0 --numeric-owner -C "$stage" -czf "$out/$agent" maxa-device
agent_sha256=$(sha256sum "$out/$agent" | cut -d' ' -f1)
sed -e "s/@VERSION@/$version/g" -e "s/@AGENT_SHA256@/$agent_sha256/g" "$root/android/agent/install.sh.in" > "$out/install.sh"
chmod 755 "$out/install.sh"
sh -n "$out/install.sh"
file "$stage/maxa-device"
(cd "$out" && sha256sum "$agent" install.sh > SHA256SUMS)
printf 'Agent payload: %s\nAgent SHA256: %s\n' "$out/$agent" "$agent_sha256"

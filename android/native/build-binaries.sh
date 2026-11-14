#!/usr/bin/env bash
# Builds restic and rclone for Android from pinned source releases and installs
# them as librestic.so and librclone.so in a jniLibs folder.
#
# Why "lib…so": since Android 10 an app may not execute files it wrote to its
# own data directory, but it may execute what the package manager extracted to
# its native library directory. Only files named lib*.so in jniLibs end up
# there, so the two executables wear that name (with extractNativeLibs=true in
# the manifest, so they exist as real files).
#
# Why cgo: a Go binary built without cgo resolves host names with Go's own DNS
# client, which reads /etc/resolv.conf. Android has no such file, so Go falls
# back to a DNS server on localhost that does not exist and every lookup fails
# ("lookup u123456.your-storagebox.de on [::1]:53: connection refused"). With
# cgo, Go's net package on Android always uses bionic's getaddrinfo, which asks
# the system resolver. So both binaries are linked with the NDK's clang against
# the minSdk API level. REPLIQUE_NATIVE_CGO=0 builds without cgo, only to
# reproduce that failure.
#
# Usage: build-binaries.sh <jniLibs-dir> [abi…]   (default ABI: arm64-v8a)
# Needs: Go (any version at least as new as the sources ask for), curl, the
# Android NDK (ANDROID_NDK_HOME, or $ANDROID_HOME/ndk/$NDK_VERSION).
set -euo pipefail

RESTIC_VERSION=0.19.1
RESTIC_SHA256=bb9b1a19040744d26d8a79be029d4e6b189c45ccc9d8831d7fe367d3c33df725
RCLONE_VERSION=1.76.0
RCLONE_SHA256=fa2b2e4ae56c4f1b91102285b736596e2a028be77cb2aaa5a0df53f3a7fc5902
NDK_VERSION=29.0.14206865
# The API level the binaries link against; must not exceed the app's minSdk,
# or the dynamic linker refuses them on older phones.
API_LEVEL=26

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:?usage: build-binaries.sh <jniLibs-dir> [abi…]}"
shift
abis=("$@")
[ ${#abis[@]} -gt 0 ] || abis=(arm64-v8a)
work="${REPLIQUE_NATIVE_WORK:-$here/../build/native}"
cgo="${REPLIQUE_NATIVE_CGO:-1}"

find_ndk() {
  local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
  for candidate in "${ANDROID_NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}" "$sdk/ndk/$NDK_VERSION" "$sdk/ndk-bundle"; do
    if [ -n "$candidate" ] && [ -d "$candidate/toolchains/llvm/prebuilt" ]; then
      echo "$candidate"
      return
    fi
  done
  echo "Android NDK not found: set ANDROID_NDK_HOME or install ndk;$NDK_VERSION with sdkmanager" >&2
  exit 1
}

# Downloads a release tarball once, checks it against its pinned hash, and
# unpacks it fresh, so a stale or tampered tree never gets built.
fetch() {
  local name="$1" url="$2" sha="$3" archive="$work/dl/$1.tar.gz"
  mkdir -p "$work/dl" "$work/src"
  if [ ! -f "$archive" ]; then
    curl -fsSL --retry 3 -o "$archive.part" "$url"
    mv "$archive.part" "$archive"
  fi
  local actual
  actual="$(shasum -a 256 "$archive" | cut -d' ' -f1)"
  if [ "$actual" != "$sha" ]; then
    echo "$archive: sha256 $actual, expected $sha" >&2
    rm -f "$archive"
    exit 1
  fi
  rm -rf "$work/src/$name"
  tar -xzf "$archive" -C "$work/src"
}

fetch "restic-$RESTIC_VERSION" \
  "https://github.com/restic/restic/releases/download/v$RESTIC_VERSION/restic-$RESTIC_VERSION.tar.gz" \
  "$RESTIC_SHA256"
fetch "rclone-v$RCLONE_VERSION" \
  "https://github.com/rclone/rclone/releases/download/v$RCLONE_VERSION/rclone-v$RCLONE_VERSION.tar.gz" \
  "$RCLONE_SHA256"
mkdir -p "$work/src/rclone-v$RCLONE_VERSION/cmd/replique-rclone"
cp "$here/rclone/main.go" "$work/src/rclone-v$RCLONE_VERSION/cmd/replique-rclone/main.go"

if [ "$cgo" = 1 ]; then
  ndk="$(find_ndk)"
  host_tag="$(ls "$ndk/toolchains/llvm/prebuilt" | head -n 1)"
  clang_dir="$ndk/toolchains/llvm/prebuilt/$host_tag/bin"
fi

# Use the Go on PATH as is: the sources' go.mod "toolchain" lines would
# otherwise make Go download another toolchain in the middle of an APK build.
export GOTOOLCHAIN=local
export GOFLAGS="${GOFLAGS:-} -trimpath"

for abi in "${abis[@]}"; do
  case "$abi" in
    arm64-v8a) goarch=arm64 triple=aarch64-linux-android ;;
    x86_64) goarch=amd64 triple=x86_64-linux-android ;;
    *) echo "unsupported ABI $abi" >&2; exit 1 ;;
  esac
  dest="$out/$abi"
  mkdir -p "$dest"
  if [ "$cgo" = 1 ]; then
    export CGO_ENABLED=1 CC="$clang_dir/$triple$API_LEVEL-clang"
  else
    export CGO_ENABLED=0
    unset CC
  fi
  echo "building restic $RESTIC_VERSION and rclone $RCLONE_VERSION for $abi (cgo=$cgo)" >&2
  (
    cd "$work/src/restic-$RESTIC_VERSION"
    GOOS=android GOARCH="$goarch" go build -ldflags "-s -w -buildid=" \
      -o "$dest/librestic.so.tmp" ./cmd/restic
  )
  (
    cd "$work/src/rclone-v$RCLONE_VERSION"
    GOOS=android GOARCH="$goarch" go build \
      -ldflags "-s -w -buildid= -X github.com/rclone/rclone/fs.Version=v$RCLONE_VERSION" \
      -o "$dest/librclone.so.tmp" ./cmd/replique-rclone
  )
  mv "$dest/librestic.so.tmp" "$dest/librestic.so"
  mv "$dest/librclone.so.tmp" "$dest/librclone.so"
done

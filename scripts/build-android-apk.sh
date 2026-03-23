#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-$HOME/.local/android-sdk}"
ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$HOME/.local/android/android-ndk-r27d}"
JAVA_HOME="${JAVA_HOME:-$HOME/.local/jdk/jdk-21.0.8+9}"
GRADLE_VERSION="${GRADLE_VERSION:-8.10.2}"
GRADLE_ROOT="${GRADLE_ROOT:-$HOME/.local/gradle}"
GRADLE_HOME="$GRADLE_ROOT/gradle-$GRADLE_VERSION"
SDKMANAGER="$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/sdkmanager"
ANDROID_ABI="${ANDROID_ABI:-arm64-v8a}"
RUST_TARGET="${RUST_TARGET:-aarch64-linux-android}"
ANDROID_BUILD_TYPE="${ANDROID_BUILD_TYPE:-release}"
ANDROID_API_LEVEL="${ANDROID_API_LEVEL:-35}"
APK_OUTPUT_NAME="${APK_OUTPUT_NAME:-linuxdo-accelerator-android-${ANDROID_ABI}.apk}"
APK_OUTPUT_DIR="${APK_OUTPUT_DIR:-$REPO_ROOT/android/dist}"

if [[ "$ANDROID_ABI" != "arm64-v8a" && "$ANDROID_ABI" != "x86_64" ]]; then
  echo "unsupported ANDROID_ABI: $ANDROID_ABI" >&2
  exit 1
fi

if [[ "$RUST_TARGET" != "aarch64-linux-android" && "$RUST_TARGET" != "x86_64-linux-android" ]]; then
  echo "unsupported RUST_TARGET: $RUST_TARGET" >&2
  exit 1
fi

if [[ ! -x "$JAVA_HOME/bin/java" ]]; then
  echo "missing JAVA_HOME: $JAVA_HOME" >&2
  exit 1
fi

if [[ ! -x "$SDKMANAGER" ]]; then
  echo "missing sdkmanager: $SDKMANAGER" >&2
  exit 1
fi

if [[ ! -x "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/clang" ]]; then
  echo "missing Android NDK: $ANDROID_NDK_HOME" >&2
  exit 1
fi

mkdir -p "$GRADLE_ROOT"

if [[ ! -x "$GRADLE_HOME/bin/gradle" ]]; then
  archive="$GRADLE_ROOT/gradle-$GRADLE_VERSION-bin.zip"
  if [[ ! -f "$archive" ]]; then
    curl -L --fail --retry 3 \
      "https://services.gradle.org/distributions/gradle-$GRADLE_VERSION-bin.zip" \
      -o "$archive"
  fi
  rm -rf "$GRADLE_HOME"
  unzip -q -o "$archive" -d "$GRADLE_ROOT"
fi

export JAVA_HOME
export ANDROID_SDK_ROOT
export ANDROID_HOME="$ANDROID_SDK_ROOT"
export ANDROID_NDK_HOME

"$SDKMANAGER" --install \
  "platform-tools" \
  "platforms;android-${ANDROID_API_LEVEL}" \
  "build-tools;35.0.0"

cd "$REPO_ROOT"
cargo ndk -t "$ANDROID_ABI" -P "$ANDROID_API_LEVEL" build --release --bin linuxdo-accelerator

build_type_lower="$(printf '%s' "$ANDROID_BUILD_TYPE" | tr '[:upper:]' '[:lower:]')"
build_type_cap="$(printf '%s' "${build_type_lower:0:1}" | tr '[:lower:]' '[:upper:]')${build_type_lower:1}"

export LINUXDO_ANDROID_RUST_BIN="$REPO_ROOT/target/$RUST_TARGET/release/linuxdo-accelerator"
"$GRADLE_HOME/bin/gradle" -p "$REPO_ROOT/android" "assemble${build_type_cap}"

apk_source="$REPO_ROOT/android/app/build/outputs/apk/${build_type_lower}/app-${build_type_lower}.apk"
if [[ ! -f "$apk_source" ]]; then
  echo "missing APK output: $apk_source" >&2
  exit 1
fi

mkdir -p "$APK_OUTPUT_DIR"
cp "$apk_source" "$APK_OUTPUT_DIR/$APK_OUTPUT_NAME"

echo "APK ready:"
echo "$APK_OUTPUT_DIR/$APK_OUTPUT_NAME"

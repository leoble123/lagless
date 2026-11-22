#!/usr/bin/env bash
# Build the Android app: the Rust library (cargo-ndk) and the APK around it (Gradle).
#   ANDROID_HOME and ANDROID_NDK_HOME must point at the SDK and NDK r27.
# Output: dist/lagless-android.apk (repo root).
set -euo pipefail
cd "$(dirname "$0")"
export ANDROID_NDK_ROOT="${ANDROID_NDK_ROOT:-$ANDROID_NDK_HOME}"
export CMAKE_POLICY_VERSION_MINIMUM="${CMAKE_POLICY_VERSION_MINIMUM:-3.5}"

rustup target add aarch64-linux-android >/dev/null
command -v cargo-ndk >/dev/null || cargo install cargo-ndk --locked
cargo ndk -t arm64-v8a -P 26 -o app/src/main/jniLibs build --release -p lagless-android

./gradlew --no-daemon -q assembleRelease
mkdir -p ../dist
cp app/build/outputs/apk/release/app-release.apk ../dist/lagless-android.apk
echo "Built dist/lagless-android.apk"

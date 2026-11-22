#!/usr/bin/env bash
# Exercise the Android JNI bridge on Linux: build it for the host, run the Java side against
# it while a lagless sender streams a test tone. Needs a JDK.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release -p lagless-android -q
cargo build --release -q
out=$(mktemp -d)
javac -d "$out" android/test-harness/android/content/Context.java android/app/src/main/java/app/lagless/Engine.java android/test-harness/app/lagless/BridgeTest.java
target/release/lagless send --source tone > /dev/null 2>&1 &
sender=$!
trap 'kill $sender 2>/dev/null' EXIT
LAGLESS_NULL_OUTPUT=1 java -cp "$out" app.lagless.BridgeTest "$PWD/target/release/liblagless_android.so" "${1:-8}"

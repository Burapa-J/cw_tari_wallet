#!/usr/bin/env bash
# Build one platform's release libraries and zip them as
# <out>/cw_tari_wallet-<tag>-<platform>.zip.
#
# Usage: scripts/release/build.sh <android|ios|macos|linux-x86_64|linux-aarch64|windows> <tag> <out-dir>
# Run from the repo root after `cargo fetch --locked` with TARI_TARGET_NETWORK=mainnet.
# Android needs ANDROID_NDK_HOME.
set -euo pipefail

# tari_common fixes the accepted networks at compile time; a testnet-group library
# would reject the frozen parse_network(None) = MainNet default.
if [ "${TARI_TARGET_NETWORK:-}" != mainnet ]; then
  echo "error: release libraries must be built with TARI_TARGET_NETWORK=mainnet" >&2
  exit 1
fi

platform="$1"
tag="$2"
# Relative paths only: Windows Python cannot read Git Bash's /d/... paths.
out="$3"
mkdir -p "$out"
lib=rust_lib_flutter_rust_wallet
stage="target/release-stage/$platform"
rm -rf "$stage" && mkdir -p "$stage"
release_dir() { echo "target/$1/release"; }

# make release-<kind> for <target>, then check tari_common's build script (whose
# output records the network group) really compiled for MainNet.
build() {
  "${RELEASE_MAKE:-make}" "release-$1" TARGET="$2"
  local dir
  dir="$(release_dir "$2")/build"
  if ! grep -rqs --include=output --include=stdout 'Building for MainNet' "$dir" ||
     grep -rqsE --include=output --include=stdout 'Building for (TestNet|NextNet)' "$dir"; then
    echo "error: $2 was not built for the mainnet network group" >&2
    exit 1
  fi
}

case "$platform" in
  android)
    toolchain="$(echo "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*-x86_64)"
    # Same targets, ABIs and API-21 linkers as Cake's scripts/android/build_minotari.sh.
    for spec in aarch64-linux-android:arm64-v8a:aarch64-linux-android21 \
                armv7-linux-androideabi:armeabi-v7a:armv7a-linux-androideabi21 \
                x86_64-linux-android:x86_64:x86_64-linux-android21; do
      IFS=: read -r target abi clang <<<"$spec"
      triple_env="$(echo "$target" | tr '[:lower:]-' '[:upper:]_')"
      # Target-scoped so host build scripts keep the host compiler.
      export "CARGO_TARGET_${triple_env}_LINKER=$toolchain/bin/$clang-clang"
      export "CC_${target//-/_}=$toolchain/bin/$clang-clang"
      export "CXX_${target//-/_}=$toolchain/bin/$clang-clang++"
      export "AR_${target//-/_}=$toolchain/bin/llvm-ar"
      build android "$target"
      mkdir -p "$stage/jniLibs/$abi"
      cp "$(release_dir "$target")/lib$lib.so" "$stage/jniLibs/$abi/"
    done
    ;;

  ios)
    # Mirrors Cake's scripts/ios/build_minotari.sh: each staticlib becomes a
    # dynamic framework, the two simulator slices are lipo'd together, and both
    # go into one xcframework.
    export IPHONEOS_DEPLOYMENT_TARGET=12.0
    for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
      build ios "$target"
    done
    fw_dir="target/release-stage/ios-frameworks"
    rm -rf "$fw_dir"
    make_framework() { # <target> <slice-dir> <sdk> <arch> <min-os-flag> <plist-platform>
      local fw="$fw_dir/$2/$lib.framework"
      mkdir -p "$fw"
      # -force_load links every archive member, like Cake's `ar x` + *.o.
      # SQLite is rusqlite's bundled copy, so no -lsqlite3. Its sqlite3* symbols
      # stay internal so a by-name lookup (dlsym/DynamicLibrary.process()) for
      # the app's own SQLite cannot land here. Hiding sqlite3* rather than
      # allowlisting FRB symbols means a new bridge export is never hidden.
      xcrun -sdk "$3" clang -dynamiclib -arch "$4" "$5" \
        -install_name "@rpath/$lib.framework/$lib" \
        -framework CoreFoundation -framework Security -lresolv -lc++ -lz \
        -Wl,-unexported_symbol,'_sqlite3*' \
        -Wl,-force_load,"$(release_dir "$1")/lib$lib.a" -o "$fw/$lib"
      cat > "$fw/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>$lib</string>
    <key>CFBundleIdentifier</key>
    <string>com.cakewallet.minotari</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$lib</string>
    <key>CFBundlePackageType</key>
    <string>FMWK</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleSupportedPlatforms</key>
    <array>
        <string>$6</string>
    </array>
    <key>DTCompiler</key>
    <string>com.apple.compilers.llvm.clang.1_0</string>
    <key>MinimumOSVersion</key>
    <string>12.0</string>
    <key>UIDeviceFamily</key>
    <array>
        <integer>1</integer>
        <integer>2</integer>
    </array>
</dict>
</plist>
EOF
    }
    make_framework aarch64-apple-ios device iphoneos arm64 -mios-version-min=12 iPhoneOS
    make_framework aarch64-apple-ios-sim sim-arm64 iphonesimulator arm64 -mios-simulator-version-min=12 iPhoneSimulator
    make_framework x86_64-apple-ios sim-x86_64 iphonesimulator x86_64 -mios-simulator-version-min=12 iPhoneSimulator
    mkdir -p "$fw_dir/sim/$lib.framework"
    lipo -create "$fw_dir/sim-arm64/$lib.framework/$lib" "$fw_dir/sim-x86_64/$lib.framework/$lib" \
      -output "$fw_dir/sim/$lib.framework/$lib"
    cp "$fw_dir/sim-arm64/$lib.framework/Info.plist" "$fw_dir/sim/$lib.framework/"
    xcodebuild -create-xcframework \
      -framework "$fw_dir/device/$lib.framework" \
      -framework "$fw_dir/sim/$lib.framework" \
      -output "$stage/$lib.xcframework"
    ;;

  macos)
    # Cake's macOS Podfile targets 13.0.
    export MACOSX_DEPLOYMENT_TARGET=13.0
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
      build macos "$target"
    done
    lipo -create "$(release_dir aarch64-apple-darwin)/lib$lib.dylib" \
      "$(release_dir x86_64-apple-darwin)/lib$lib.dylib" -output "$stage/lib$lib.dylib"
    # rustc records the absolute build path as the install name; changing it
    # breaks the arm64 linker signature, so re-sign ad hoc.
    install_name_tool -id "@rpath/lib$lib.dylib" "$stage/lib$lib.dylib"
    codesign --force --sign - "$stage/lib$lib.dylib"
    ;;

  linux-x86_64 | linux-aarch64)
    target="${platform#linux-}-unknown-linux-gnu"
    build linux "$target"
    cp "$(release_dir "$target")/lib$lib.so" "$stage/"
    ;;

  windows)
    build windows x86_64-pc-windows-msvc
    cp "$(release_dir x86_64-pc-windows-msvc)/$lib.dll" "$stage/"
    ;;

  *)
    echo "unknown platform: $platform" >&2
    exit 1
    ;;
esac

# `python` first: on Windows runners `python3` can be the Microsoft Store stub.
python="$(command -v python || command -v python3)"
zip="$out/cw_tari_wallet-$tag-$platform.zip"
rm -f "$zip"
"$python" -m zipfile -c "$zip" "$stage"/*
echo "Wrote $zip"

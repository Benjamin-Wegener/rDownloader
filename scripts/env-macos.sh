# Build-Umgebung ohne Xcode/CLT: Homebrew-LLVM + macOS-SDK.
# Nutzung: source scripts/env-macos.sh
export PATH="$HOME/.cargo/bin:$PATH"
unset DEVELOPER_DIR
export SDKROOT="${SDKROOT_OVERRIDE:-$HOME/.local/share/SDKs/MacOSX26.0.sdk}"
LLVM=/opt/brew/opt/llvm
export PATH="/opt/brew/bin:$PATH"
export CC="$LLVM/bin/clang" CXX="$LLVM/bin/clang++" AR="$LLVM/bin/llvm-ar"
export CFLAGS="-isysroot $SDKROOT" CXXFLAGS="-isysroot $SDKROOT"
export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$CC"
export RUSTFLAGS="-C link-arg=-fuse-ld=lld -C link-arg=-isysroot -C link-arg=$SDKROOT -C link-arg=-L$SDKROOT/usr/lib${RUSTFLAGS:+ $RUSTFLAGS}"

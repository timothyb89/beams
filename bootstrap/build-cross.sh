#!/bin/bash
# Cross-compiles beam-bootstrap for Linux (statically linked, musl libc) so
# it can be shipped to a beam from a workstation that isn't itself running
# Linux - macOS, in particular. Uses cargo-zigbuild, which bundles a C/C++
# cross-linker (via zig) so no system Linux toolchain is needed locally.
#
# Also works unmodified on a Linux workstation - it just cross-links to
# musl instead of your host's glibc, which is what we want anyway (a
# statically linked binary has no glibc-version compatibility question
# against whatever a given beam image happens to ship).
#
# One-time setup, if not already present:
#   rustup target add x86_64-unknown-linux-musl
#   cargo install cargo-zigbuild
#   pip3 install ziglang        # provides a `zig` toolchain; a Homebrew
#                                # `zig` install works too - either way,
#                                # cargo-zigbuild just needs `zig` on PATH
#                                # or available via `python3 -m ziglang`.
#
# Usage:
#   bootstrap/build-cross.sh
#
# Output: bootstrap/target/x86_64-unknown-linux-musl/release/beam-bootstrap
# - this is the path client/src/stage.rs looks for first when assembling
# what to copy onto a beam.

set -euo pipefail

TARGET="x86_64-unknown-linux-musl"
cd "$(dirname "${BASH_SOURCE[0]}")"

if ! command -v cargo-zigbuild >/dev/null 2>&1; then
    echo "build-cross.sh: cargo-zigbuild not found - run: cargo install cargo-zigbuild" >&2
    exit 1
fi

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
    echo "build-cross.sh: adding missing rustup target $TARGET" >&2
    rustup target add "$TARGET"
fi

cargo zigbuild --release --target "$TARGET"

bin="target/$TARGET/release/beam-bootstrap"
echo "build-cross.sh: built $bin"
file "$bin" 2>/dev/null || true

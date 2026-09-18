#!/usr/bin/env bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Both eBPF binaries shipped by the xdp-ebpf crate. Built with debug info and BTF
# emission: BTF is required for the dispatcher's freplace slots and for loading
# the redirect program as a dispatcher member (BPF_PROG_TYPE_EXT).
BINS=(agave-xdp-prog agave-xdp-dispatcher)

declare -A BEFORE_HASH
for bin in "${BINS[@]}"; do
    BEFORE_HASH[$bin]=$(sha256sum "$REPO_ROOT/xdp-ebpf/$bin" 2>/dev/null | awk '{print $1}')
    echo "Hash before rebuild ($bin): ${BEFORE_HASH[$bin]:-<missing>}"
done

# shellcheck disable=SC1091
source "$REPO_ROOT/ci/rust-version.sh"

echo "Using nightly toolchain: $rust_nightly"

if ! command -v bpf-linker &> /dev/null; then
    echo "Installing bpf-linker..."
    cargo install bpf-linker@0.9.15
fi

rustup component add rust-src --toolchain "$rust_nightly"

# Remap absolute build paths (checkout, cargo registry, toolchain source) to stable
# placeholders so the emitted .BTF — which embeds source file paths — is identical
# across machines and does not ship the builder's $HOME. Applied via RUSTFLAGS so it
# also covers the -Z build-std core unit, not just the top crate.
export RUSTFLAGS="--remap-path-prefix=$REPO_ROOT=/src \
--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo \
--remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"

changed=0
for bin in "${BINS[@]}"; do
    cargo +"$rust_nightly" rustc --manifest-path "$REPO_ROOT/xdp-ebpf/Cargo.toml" \
        --bin "$bin" \
        --target bpfel-unknown-none --release --features agave-unstable-api,ebpf \
        -Z build-std=core -- -C debuginfo=2 -C link-arg=--btf

    # this is needed to strip FILE symbols which have paths that differ between
    # rebuilds (sections, including .BTF, are unaffected)
    llvm-objcopy --strip-unneeded "$REPO_ROOT/target/bpfel-unknown-none/release/$bin" "$REPO_ROOT/xdp-ebpf/$bin"

    AFTER_HASH=$(sha256sum "$REPO_ROOT/xdp-ebpf/$bin" | awk '{print $1}')
    echo "Hash after rebuild ($bin):  $AFTER_HASH"
    if [ "${BEFORE_HASH[$bin]}" != "$AFTER_HASH" ]; then
        changed=1
        echo "✗ Hash changed ($bin)"
    else
        echo "✓ Hash unchanged ($bin)"
    fi
done

exit $changed

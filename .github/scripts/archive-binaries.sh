#!/bin/bash

set -euo pipefail

# Configuration
TAG="${TAG:-latest}"
WORKSPACE_ROOT="${WORKSPACE_ROOT:-$(pwd)}"
TARGET_DIR="${TARGET_DIR:-$WORKSPACE_ROOT/target}"
ARCHIVE_NAME="${ARCHIVE_NAME:-solana-jito-${TAG}.tar.gz}"
OUTPUT_DIR="${OUTPUT_DIR:-$WORKSPACE_ROOT}"
TEMP_DIR=$(mktemp -d)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

cleanup() {
    if [[ -d "$TEMP_DIR" ]]; then
        rm -rf "$TEMP_DIR"
        log_info "Cleaned up temporary directory: $TEMP_DIR"
    fi
}

trap cleanup EXIT

# Validate workspace
if [[ ! -f "$WORKSPACE_ROOT/Cargo.toml" ]]; then
    log_error "No Cargo.toml found in workspace root: $WORKSPACE_ROOT"
    exit 1
fi

log_info "Starting binary archiving process..."
log_info "Workspace root: $WORKSPACE_ROOT"
log_info "Target directory: $TARGET_DIR"
log_info "Archive name: $ARCHIVE_NAME"

# Find binary directory based on profile
BIN_DIR="$TARGET_DIR/release"

if [[ ! -d "$BIN_DIR" ]]; then
    log_error "Binary directory not found: $BIN_DIR"
    exit 1
fi

log_info "Looking for binaries in: $BIN_DIR"

# Create staging directory
STAGING_DIR="$TEMP_DIR/bin"
mkdir -p "$STAGING_DIR"

# Find all executable binaries (exclude shared libraries and other files)
BINARY_COUNT=0

# Get list of package names from workspace
PACKAGES=$(cargo metadata --no-deps --format-version 1 | \
    jq -r '.packages[].targets[] | select(.kind[] | contains("bin")) | .name' 2>/dev/null || true)

# Look for binaries matching package names
for package in $PACKAGES; do
    binary_path="$BIN_DIR/$package"
    if [[ -f "$binary_path" && -x "$binary_path" ]]; then
        log_info "Copying binary: $package"
        cp "$binary_path" "$STAGING_DIR/"
        BINARY_COUNT=$((BINARY_COUNT + 1))
    fi
done

if [[ $BINARY_COUNT -eq 0 ]]; then
    log_error "No binaries found in $BIN_DIR"
    log_error "Make sure you have built the project with the correct profile"
    exit 1
fi

log_success "Found $BINARY_COUNT binaries"

# Copy performance libraries
log_info "Copying performance libraries..."
cp -rL "$BIN_DIR/perf-libs" "$STAGING_DIR"
log_success "Copied performance libraries"

# Generate version.yml
{
  echo "channel: $TAG"
  echo "commit: $(git rev-parse HEAD)"
  echo "target: x86_64-unknown-linux-gnu"
} > "$TEMP_DIR/version.yml"
log_success "Generated version.yml"

# Create the archive
ARCHIVE_PATH="$OUTPUT_DIR/$ARCHIVE_NAME"
log_info "Creating archive: $ARCHIVE_PATH"

cd "$TEMP_DIR"
tar -czf "$ARCHIVE_PATH" .

if [[ ! -f "$ARCHIVE_PATH" ]]; then
    log_error "Failed to create archive"
    exit 1
fi

# Get archive size
ARCHIVE_SIZE=$(du -h "$ARCHIVE_PATH" | cut -f1)
log_success "Archive created successfully: $ARCHIVE_NAME ($ARCHIVE_SIZE)"

# List contents for verification
log_info "Archive contents:"
tar -tzf "$ARCHIVE_PATH" | sed 's/^/  /'

# Rename install binary
mv "$BIN_DIR/agave-install-init" "$BIN_DIR/agave-install-init-x86_64-unknown-linux-gnu"

# Output for GitHub Actions
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    {
      echo "archive-path=$ARCHIVE_PATH"
      echo "archive-name=$ARCHIVE_NAME"
      echo "binary-count=$BINARY_COUNT"
      echo "archive-size=$ARCHIVE_SIZE"
      echo "install-init=$BIN_DIR/agave-install-init-x86_64-unknown-linux-gnu"
    } >> "$GITHUB_OUTPUT"
fi

log_success "Binary archiving completed successfully!"

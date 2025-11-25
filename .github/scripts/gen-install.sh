#!/bin/bash

set -euo pipefail

TAG="${TAG:-latest}"
WORKSPACE_ROOT="${WORKSPACE_ROOT:-$(pwd)}"
OUTPUT_DIR="${OUTPUT_DIR:-$WORKSPACE_ROOT}"
OUTPUT_FILE="${OUTPUT_DIR}/install.sh"

echo "SOLANA_RELEASE=${TAG}" > "${OUTPUT_FILE}"
echo "SOLANA_INSTALL_INIT_ARGS=${TAG}" >> "${OUTPUT_FILE}"
cat "${WORKSPACE_ROOT}/install/agave-install-init.sh" >> "${OUTPUT_FILE}"

chmod +x "${OUTPUT_FILE}"

# Output for GitHub Actions
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    {
      echo "install=${OUTPUT_FILE}"
    } >> "$GITHUB_OUTPUT"
fi

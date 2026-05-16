#!/usr/bin/env bash
# Regenerate card-snapshots-linux/ by running the snapshot test inside an
# Alpine container that matches the production Dockerfile's runtime fonts
# (Noto CJK + Noto Color Emoji). Useful to verify rendering on the same
# font stack the deployed bot uses.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="$REPO_ROOT/card-snapshots-linux"
mkdir -p "$OUTPUT_DIR"

docker run --rm \
    -v "$REPO_ROOT:/src:ro" \
    -v "$OUTPUT_DIR:/output" \
    rust:1.94-alpine \
    sh -c '
        set -eu
        apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig \
            font-noto-cjk font-noto-emoji
        cp -r /src /work
        cd /work
        cargo test --lib generate_snapshots
        cp card-snapshots/*.png /output/
    '

echo "Snapshots written to $OUTPUT_DIR"

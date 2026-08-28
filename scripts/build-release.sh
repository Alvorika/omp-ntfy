#!/usr/bin/env bash
# Build an omp-ntfy release artifact: binary + docs, one tarball + sha256.
#
# Native build:    ./scripts/build-release.sh              (glibc, this platform)
# Static musl:     ./scripts/build-release.sh --musl       (needs musl toolchain)
# Static musl via Docker (no local toolchain needed):
#                  docker build -t omp-ntfy:build .
#                  docker run --rm -v "$PWD/dist:/out" omp-ntfy:build \
#                    sh -c 'cp /usr/local/bin/omp-ntfy /out/omp-ntfy-musl'
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
TARGET="x86_64-unknown-linux-gnu"

if [[ "${1:-}" == "--musl" ]]; then
    TARGET="x86_64-unknown-linux-musl"
    rustup target add "$TARGET" >/dev/null 2>&1 || true
fi
EXTRA="--target $TARGET"

echo "Building omp-ntfy v$VERSION ($TARGET)…"
cargo build --release --locked $EXTRA

mkdir -p dist
STAGE="dist/omp-ntfy-$VERSION"
rm -rf "$STAGE"
mkdir -p "$STAGE"

BIN="target/$TARGET/release/omp-ntfy"
cp "$BIN" "$STAGE/omp-ntfy"
chmod +x "$STAGE/omp-ntfy"
cp LICENSE README.md CHANGELOG.md "$STAGE/"
cp compose.yaml Dockerfile docker-entrypoint.sh "$STAGE/" 2>/dev/null || true

cat > "$STAGE/install.sh" <<'EOF'
#!/usr/bin/env bash
# Install omp-ntfy into ~/.local/bin (no root). Then run `omp-ntfy init`.
set -euo pipefail
install -d "$HOME/.local/bin"
install -m755 "$(dirname "$0")/omp-ntfy" "$HOME/.local/bin/omp-ntfy"
echo "installed $(command -v omp-ntfy || echo "$HOME/.local/bin/omp-ntfy")"
EOF
chmod +x "$STAGE/install.sh"

(
    cd dist
    tar czf "omp-ntfy-$VERSION-$TARGET.tar.gz" "omp-ntfy-$VERSION"
    sha256sum "omp-ntfy-$VERSION-$TARGET.tar.gz" > "omp-ntfy-$VERSION-$TARGET.tar.gz.sha256"
)

echo ""
echo "artifact: dist/omp-ntfy-$VERSION-$TARGET.tar.gz"
cat "dist/omp-ntfy-$VERSION-$TARGET.tar.gz.sha256"
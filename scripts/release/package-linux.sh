#!/usr/bin/env bash
# Empaquetado reproducible de Aeril para Linux x86_64 (PA0-D / E2E-1).
#
# Produce un tarball y SHA-256 deterministas independientes del path de build
# y de la hora de reloj (usando SOURCE_DATE_EPOCH y --remap-path-prefix).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

OUT_DIR="${OUT_DIR:-$ROOT/dist}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
JOBS="${JOBS:-2}"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"

if [[ -z "${SOURCE_DATE_EPOCH:-}" ]]; then
  if command -v git >/dev/null 2>&1 && git rev-parse HEAD >/dev/null 2>&1; then
    SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)"
  else
    SOURCE_DATE_EPOCH="1760000000"
  fi
fi
export SOURCE_DATE_EPOCH
export TZ=UTC
export LC_ALL=C
export CARGO_INCREMENTAL=0

VERSION="$(awk -F'"' '/^version =/{print $2; exit}' Cargo.toml)"
ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
  echo "package-linux: arquitectura '$ARCH' no soportada por PA0 (solo x86_64)" >&2
  exit 2
fi

PKG_NAME="aeril-${VERSION}-linux-${ARCH}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

REMAP_RUST="--remap-path-prefix=${ROOT}=. --remap-path-prefix=${CARGO_HOME_DIR}=/cargo"
REMAP_C="-ffile-prefix-map=${ROOT}=. -ffile-prefix-map=${CARGO_HOME_DIR}=/cargo -g0"

export RUSTFLAGS="${RUSTFLAGS:-} ${REMAP_RUST}"
export CFLAGS="${CFLAGS:-} ${REMAP_C}"

CARGO_ARGS=(build --release --locked --jobs "$JOBS")
if [[ "${AERIL_OFFLINE:-0}" == "1" ]]; then
  CARGO_ARGS+=(--offline)
fi

CARGO_TARGET_DIR="$TARGET_DIR" cargo "${CARGO_ARGS[@]}"

BIN="$TARGET_DIR/release/aeril"
"$ROOT/scripts/budget-size.sh" "$BIN"
"$ROOT/scripts/budget-deps.sh" "$BIN"

VER_OUT="$("$BIN" version)"
if [[ "$VER_OUT" != "aeril ${VERSION}" ]]; then
  echo "package-linux: 'aeril version' devolvió '${VER_OUT}', se esperaba 'aeril ${VERSION}'" >&2
  exit 1
fi

mkdir -p "$OUT_DIR" "$STAGE/$PKG_NAME"
cp "$BIN" "$STAGE/$PKG_NAME/aeril"
chmod 0755 "$STAGE/$PKG_NAME/aeril"
touch -d "@${SOURCE_DATE_EPOCH}" "$STAGE/$PKG_NAME/aeril"

for doc in README.md HANDOFF.md docs/pa0-alpha-contract.md; do
  if [[ -f "$ROOT/$doc" ]]; then
    base="$(basename "$doc")"
    cp "$ROOT/$doc" "$STAGE/$PKG_NAME/$base"
    chmod 0644 "$STAGE/$PKG_NAME/$base"
    touch -d "@${SOURCE_DATE_EPOCH}" "$STAGE/$PKG_NAME/$base"
  fi
done
touch -d "@${SOURCE_DATE_EPOCH}" "$STAGE/$PKG_NAME"

BIN_SHA="$(sha256sum "$STAGE/$PKG_NAME/aeril" | awk '{print $1}')"
TARBALL="$OUT_DIR/${PKG_NAME}.tar.gz"

tar --sort=name \
  --mtime="@${SOURCE_DATE_EPOCH}" \
  --owner=0 --group=0 --numeric-owner \
  -C "$STAGE" -cf - "$PKG_NAME" | gzip -n > "$TARBALL"

TAR_SHA="$(sha256sum "$TARBALL" | awk '{print $1}')"
(cd "$OUT_DIR" && printf "%s  %s\n" "$TAR_SHA" "$(basename "$TARBALL")" > "${PKG_NAME}.tar.gz.sha256")

echo "bin_sha256=${BIN_SHA}"
echo "tarball=${TARBALL}"
echo "tarball_sha256=${TAR_SHA}"
echo "source_date_epoch=${SOURCE_DATE_EPOCH}"

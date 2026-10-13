#!/usr/bin/env bash
# Presupuesto de dependencias de runtime: 0 dependencias de app.
# El binario solo puede enlazar ABI base del sistema. Whitelist declarada:
#   - linux-vdso, ld-linux-x86-64, libc, libm, libgcc_s, libpthread,
#     libdl, librt  → glibc/ABI del OS.
#   - libz.so.1 → zlib del sistema es parte de la base de Ubuntu (Priority:
#     required). Se acepta y se reporta explícitamente.
# Cualquier otra biblioteca es FAIL y exige justificación escrita.
set -euo pipefail
BIN="${1:-target/release/aeril}"
if [[ ! -f "$BIN" ]]; then
  echo "no existe $BIN (corre: make build)" >&2
  exit 2
fi
echo "--- ldd $BIN ---"
ldd "$BIN"
echo "----------------"
ALLOWED='^(linux-vdso|ld-linux|libc\.so|libm\.so|libgcc_s\.so|libpthread\.so|librt\.so|libdl\.so|libz\.so)'
BAD=$(ldd "$BIN" | awk '/=>/ {print $1}' | grep -Ev "$ALLOWED" || true)
if [[ -n "$BAD" ]]; then
  echo "budget-deps: FAIL — dependencias fuera de la base:"
  echo "$BAD"
  exit 1
fi
echo "budget-deps: PASS (solo ABI base del OS + zlib del sistema)"

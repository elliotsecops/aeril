#!/usr/bin/env bash
# Presupuesto de tamaño del binario: ≤ 30 MB.
# Definición del gate: MB = 1e6 bytes (decimal, más estricto que MiB).
set -euo pipefail
BIN="${1:-target/release/aeril}"
LIMIT=$((30 * 1000 * 1000))
if [[ ! -f "$BIN" ]]; then
  echo "no existe $BIN (corre: make build)" >&2
  exit 2
fi
SIZE=$(stat -c%s "$BIN")
MB=$(awk -v s="$SIZE" 'BEGIN{printf "%.1f", s/1000000}')
echo "binario: $SIZE bytes ($MB MB) · límite 30 MB (decimal)"
if (( SIZE > LIMIT )); then
  echo "budget-size: FAIL"
  exit 1
fi
echo "budget-size: PASS"

# Aeril — Makefile de la Fase 0.
#
# Presupuestos (MANIFIESTO §2, v0.4):
#   binario ≤ 30 MB (decimal) · RSS idle ≤ 50 MB · arranque p95 ≤ 300 ms
#   (entrada procesada, medido desde fuera del proceso).
# `make budget` corre los presupuestos controlables en esta máquina.
# `budget-scale` (5 agentes) NO se puede verificar en Fase 0: reporta
# UNVERIFIED y falla a propósito hasta que exista la medición real.

CARGO ?= cargo
PYTHON ?= python3
BIN := target/release/aeril
DATA_DIR ?= bench-data
BENCH_DIR := $(DATA_DIR)/bench
JOBS ?= 2

.PHONY: all build test fmt clippy bench-seed budget budget-size budget-deps \
        budget-mem budget-start budget-scale clean

all: build

build:
	CARGO_BUILD_JOBS=$(JOBS) $(CARGO) build --release

test:
	CARGO_BUILD_JOBS=$(JOBS) $(CARGO) test --jobs $(JOBS)

fmt:
	$(CARGO) fmt

clippy:
	CARGO_BUILD_JOBS=$(JOBS) $(CARGO) clippy --all-targets -- -D warnings

bench-seed: $(BIN)
	@mkdir -p $(DATA_DIR)
	$(BIN) bench seed --dir $(BENCH_DIR) --threads 100 --events 1000 --diffs 10 | tee $(DATA_DIR)/seed.json

$(BIN):
	$(MAKE) build

budget-size: $(BIN)
	@scripts/budget-size.sh $(BIN)

budget-deps: $(BIN)
	@scripts/budget-deps.sh $(BIN)

budget-mem: $(BIN) bench-seed
	@$(PYTHON) scripts/bench/startup.py --bin $(BIN) --data-dir $(BENCH_DIR) \
		--runs 1 --mem --json | tee $(DATA_DIR)/mem.json

budget-start: $(BIN) bench-seed
	@$(PYTHON) scripts/bench/startup.py --bin $(BIN) --data-dir $(BENCH_DIR) \
		--runs 30 --json | tee $(DATA_DIR)/start.json
	@$(PYTHON) scripts/bench/startup.py --bin $(BIN) --data-dir $(BENCH_DIR) \
		--runs 30 | tee $(DATA_DIR)/start.txt

# 5 agentes ≤ 500 MB: no verificable en Fase 0 (un solo backend/agente).
budget-scale: $(BIN)
	@echo "budget-scale: UNVERIFIED — Fase 0 solo mide un agente; ejecutar en Fase 1."
	@exit 1

budget: budget-size budget-deps budget-mem budget-start

clean:
	$(CARGO) clean
	rm -rf $(DATA_DIR)

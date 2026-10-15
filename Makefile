# Aeril — Makefile de la Fase 0.
#
# Presupuestos (MANIFIESTO §2, v0.4):
#   binario ≤ 30 MB (decimal) · RSS idle ≤ 50 MB · arranque p95 ≤ 300 ms
#   (entrada procesada, medido desde fuera del proceso).
# `make budget` corre los presupuestos controlables en esta máquina.
# `budget-scale` mide 5 runs mock concurrentes (app ≤ 500 MB; agentes y total
# aparte; NO prueba Hermes real). `budget-two` (S6) mide el escenario real de
# 2 runs: RSS de TUI/mocks, arranque con runs activos y latencia de consultas.

CARGO ?= cargo
PYTHON ?= python3
BIN := target/release/aeril
DATA_DIR ?= bench-data
BENCH_DIR := $(DATA_DIR)/bench
JOBS ?= 2

.PHONY: all build test fmt clippy budget budget-size budget-deps \
        budget-mem budget-start budget-scale budget-two clean

all: build

build:
	CARGO_BUILD_JOBS=$(JOBS) $(CARGO) build --release

test:
	CARGO_BUILD_JOBS=$(JOBS) $(CARGO) test --jobs $(JOBS)

fmt:
	$(CARGO) fmt

clippy:
	CARGO_BUILD_JOBS=$(JOBS) $(CARGO) clippy --all-targets -- -D warnings

$(BIN):
	$(MAKE) build

# El seed corre una sola vez por binario (stamp): re-seed si el binario cambió.
BENCH_STAMP := $(DATA_DIR)/.seeded
bench-seed: $(BENCH_STAMP)

$(BENCH_STAMP): $(BIN)
	@mkdir -p $(DATA_DIR)
	@$(BIN) bench seed --dir $(BENCH_DIR) --threads 100 --events 1000 --diffs 10 > $(DATA_DIR)/seed.json
	@cat $(DATA_DIR)/seed.json
	@touch $(BENCH_STAMP)

budget-size: $(BIN)
	@scripts/budget-size.sh $(BIN)

budget-deps: $(BIN)
	@scripts/budget-deps.sh $(BIN)

budget-mem: $(BIN) bench-seed
	@$(PYTHON) scripts/bench/startup.py --bin $(BIN) --data-dir $(BENCH_DIR) \
		--runs 1 --mem --json > $(DATA_DIR)/mem.json
	@cat $(DATA_DIR)/mem.json

budget-start: $(BIN) bench-seed
	@$(PYTHON) scripts/bench/startup.py --bin $(BIN) --data-dir $(BENCH_DIR) \
		--runs 30 --json > $(DATA_DIR)/start.json
	@cat $(DATA_DIR)/start.json

# Escenario real de 2 runs mock concurrentes (S6 §3): RSS de la TUI, RSS de
# cada backend mock por separado, arranque de una TUI con 2 runs activos y
# latencia de consultas. Data dir DESCARTABLE: se recrea en cada corrida.
# NO prueba Hermes real (los mocks no son Hermes).
budget-two: $(BIN)
	@$(PYTHON) scripts/bench/two_run.py --bin $(BIN) --data-dir $(DATA_DIR)/two-run \
		--json > $(DATA_DIR)/two_run.json
	@cat $(DATA_DIR)/two_run.json

# 5 agentes: gate de la APP ≤ 500 MB con runs mock concurrentes; agentes y
# total medidos aparte. NO prueba Hermes real (perfil compartido, no medible).
budget-scale: $(BIN)
	@$(BIN) bench scale --runs 5 --delay-ms 6000 --data-dir $(DATA_DIR)/scale-data > $(DATA_DIR)/scale.json
	@cat $(DATA_DIR)/scale.json

budget: budget-size budget-deps budget-mem budget-start

clean:
	$(CARGO) clean
	rm -rf $(DATA_DIR)

# Aeril

**A lightweight workspace for AI coding agents, right in your terminal.**

Keep track of what your AI coding assistants are doing, review what they changed, leave feedback, and pick up cleanly where you left off—without bloated editors or juggling dozens of terminal windows.

> **Built for modest hardware.** Aeril itself uses very little memory, so you can run it smoothly on a basic laptop, VM, or low-cost Linux VPS. *(Keep in mind that external AI models and agent tools have their own memory and API requirements.)*

![Aeril main view: task pane, run details, and verified checkpoint](docs/assets/aeril-tui-preview.png)

---

## Why Aeril? (Product Overview)

Suppose you are building an app and ask an AI assistant to fix a bug. Afterward, you want to see exactly which lines changed, leave a note about something it missed, and continue the next day. Without a workspace manager, that quickly turns into scattered terminal tabs, lost progress, and guessing what the AI edited.

Aeril acts like a clean **control room** for those AI tasks:

- **One place per task:** Every task keeps its own conversation history, workspace, and saved progress—no more digging through old terminal scrollback.
- **See changes before keeping them:** Inspect what the AI modified and approve it before anything touches your main project files.
- **Give feedback and iterate:** Leave notes directly on specific changes so the AI knows what to adjust on the next pass.
- **Recover cleanly from interruptions:** If your terminal closes or your computer restarts unexpectedly, Aeril remembers where each task stopped and keeps your verified file snapshots safe.
- **Stay out of each other's way:** Prevents two sessions from accidentally editing the same workspace at once.
- **Works alongside your tools:** Your project stays a normal folder/Git repository—use whatever code editor you already prefer.

> **Important:** Aeril is **not** an AI model itself. It is the workspace that organizes and supervises external AI coding agents (such as [Hermes](docs/acp-hermes-spec.md)).

### How It Works

![Aeril demo: task navigation, mock agent execution, and checkpoint](docs/assets/aeril-tui-demo.gif)

1. **Open a task.** Each task gets its own history, branch, and isolated Git workspace.
2. **Start an agent.** Aeril supervises the run and displays the objective, live activity, and execution status.
3. **Inspect the result.** Review the diff, modified files, and verified checkpoint before integrating anything.
4. **Leave context.** Comments and feedback remain anchored to the task and the reviewed checkpoint for follow-up runs.
5. **Resume safely.** If a session is interrupted, Aeril preserves durable evidence and recovery points without rebuilding state from dirty working-tree assumptions.

---

## Technical Capabilities

For engineers and contributors, here is how Aeril enforces those guarantees under the hood:

- **Isolated task workspaces:** Each task thread runs in its own verified Git worktree (`aeril/ws/<id>`) with an independent event log.
- **Immutable checkpoints:** Captures real working-tree diffs into Git snapshot refs (`refs/aeril/checkpoints/<id>`) via a crash-safe `prepared → ready` protocol without modifying your active branch or index.
- **Anchored code review:** Attaches line-level comments to verified checkpoints and sends structured feedback turns back to the engine.
- **Crash recovery & workspace safety:** Reconciles interrupted runs on startup without fabricating state from the current working tree, prevents concurrent Aeril writers via PID-verified advisory leases, and restricts integration to fast-forward CAS updates.
- **Low-overhead architecture:** Single static Rust binary using a synchronous `ratatui` TUI, a local SQLite WAL store, and an explicit ACP stdio client (`tokio`).

The TUI supports supervising up to two concurrent mock runs. Live Hermes execution enforces a single authenticated session at a time.

---

## Getting Started

> **Status:** Early developer preview. Aeril currently requires building from source on Linux and has no packaged installer or universal compatibility guarantee yet.

### Prerequisites

- Linux (tested on x86_64)
- Git
- Rust & Cargo (2024 edition)
- A compatible external ACP agent (e.g., `hermes acp`) for live AI runs, or the built-in mock backend for development

### Build & Run

```bash
git clone https://github.com/elliotsecops/aeril.git
cd aeril
cargo build --release
./target/release/aeril
```

Running `aeril` without arguments launches the terminal UI. To register a Git repository and inspect threads from the CLI:

```bash
./target/release/aeril thread new --repo /path/to/your/repo --title "Fix login flow"
./target/release/aeril thread list
```

*(Creating a thread registers the task only; it does not start an agent run automatically.)*

---

## Current Status & Limitations

Aeril already includes the terminal UI, durable task history, diff inspection, anchored review comments, immutable checkpoints, crash recovery, and workspace coordination. Final TUI integration passed **230/230 tests** across three consecutive runs, and release resource budgets pass.

Managed Hermes profile isolation (P2-S1) is also implemented, with per-agent `HERMES_HOME` isolation and single-writer locks, and remains **disabled by default** (`AERIL_HERMES_PROFILES`).

### Not Yet Supported / Out of Scope

- **Concurrent authenticated Hermes sessions:** Running two live authenticated Hermes sessions simultaneously is not supported (`P2-S2` remains on HOLD; the TUI enforces one active Hermes run at a time).
- **OS sandboxing & host credential isolation:** Aeril separates tasks and workspaces cooperatively; it is not an OS security sandbox isolating files or ambient credentials from processes running as the same user.
- **Verified conversational memory restore:** Checkpoints recover verified project state (`verified=false` on ACP `session/load` restoration), not guaranteed model memory or private reasoning.
- **Certified hardware/distro matrix:** Raspberry Pi (ARM), Android (Termux), and arbitrary VPS tiers are not certified.
- **Agent memory containment:** An external AI agent's own memory usage is independent and does not fit inside Aeril's idle memory budget.

---

## Target Environments

| Environment | Status |
|---|---|
| Linux desktop / laptop (x86_64) | Primary development and test environment |
| Linux VM | Supported (build from source) |
| Low-cost Linux VPS | Design target (external agent/model RAM and API costs are separate) |
| Raspberry Pi (ARM) | Target interest; uncertified |
| Android (Termux) | Potential future target; untested and unsupported |

---

## Architecture & Performance

- **Stack:** Rust, `ratatui` (TUI), `rusqlite` (SQLite WAL), `git2` (libgit2 worktrees/snapshots/CAS), `tokio` (per-run ACP stdio client).
- **Measured orchestrator footprint** (release build on Linux x86_64 — Intel i7-8550U, 15.4 GiB RAM, warm OS page cache; binary SHA-256 `64ed48a52a558e216bc400ab550759e30936bc3e77dab53226ce5eab71f3618c`):

| Metric | Measured |
|---|---:|
| Release binary size | **5,928,496 bytes** (5.93 MB) |
| Idle RSS | **7,276 KiB** (7.1 MiB) |
| RSS with one thread open | **8,248 KiB** (8.1 MiB) |
| Additional memory per thread | **0.95 MiB** |
| Startup latency (p95) | **22.21 ms** |
| TUI with two mock runs | **8,912 KiB** |
| Combined RSS, TUI + two mocks | **18,052 KiB** |
| Rust test suite | **230/230 PASS** (three consecutive runs) |
| `cargo fmt` / Clippy | PASS |

*These measurements reflect a specific hardware environment and are not guaranteed minimums. Memory used by Hermes or any external agent is **not** included in Aeril's idle RSS.*

---

## Documentation

- [Phase 2 TUI Acceptance & Concurrency Evidence](docs/phase2-tui-acceptance.md)
- [Manifesto & Performance Budgets](MANIFIESTO.md)
- [Engineering Handoff & Current Limits](HANDOFF.md)
- [Phase 1 Status & Verification](docs/phase1-status.md)
- [Phase 2-S1 Managed Hermes Profiles](docs/phase2-s1-implementation.md)
- [Phase 2-S1 R3 Audit Remediation](docs/phase2-s1-r3-remediation.md)
- [Hermes ACP Protocol Specification](docs/acp-hermes-spec.md)

---

**Aeril — fewer windows, more control over your agents' work.**

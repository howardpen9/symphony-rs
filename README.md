# symphony-rs

Rust rewrite of [openai/symphony](https://github.com/openai/symphony), implemented against the public `SPEC.md` and the Elixir reference behavior.

This project is a long-running Symphony service, not just a parser scaffold. It includes:

- `WORKFLOW.md` loader with YAML front matter parsing
- typed config defaults, env indirection, and dispatch validation
- Linear GraphQL client and tracker adapter
- workspace management with path-safety checks and lifecycle hooks
- Codex app-server client over stdio
- agent runner with continuation turns and `linear_graphql` dynamic tool support
- orchestrator state for polling, claims, retries, reconciliation, and token accounting
- CLI commands for `validate`, `snapshot`, `once`, and `serve`

## Commands

```bash
cd /Users/smartchoice/Projects/symphony-rs

cargo run -- validate
cargo run -- snapshot
cargo run -- once
cargo run -- serve --i-understand-that-this-will-be-running-without-the-usual-guardrails
```

If you want Symphony to talk to Linear, export at least:

```bash
export LINEAR_API_KEY=...
```

Then set `tracker.project_slug` in [WORKFLOW.md](/Users/smartchoice/Projects/symphony-rs/WORKFLOW.md).

## Layout

- `src/workflow.rs`: `WORKFLOW.md` parsing
- `src/config.rs`: typed workflow config and runtime defaults
- `src/linear.rs`: Linear GraphQL integration and issue normalization
- `src/workspace.rs`: workspace creation, cleanup, and hooks
- `src/codex.rs`: Codex app-server session client
- `src/runner.rs`: single-issue worker attempt runner
- `src/orchestrator.rs`: dispatch eligibility, retries, and runtime state
- `src/service.rs`: long-running service loop

## Verification

```bash
cargo test
```

Current local smoke checks:

- `cargo test` passes
- `cargo run -- validate` passes
- `cargo run -- snapshot` passes

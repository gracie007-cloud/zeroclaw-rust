# AGENTS.md

This document is for agents working in this repository.
Goal: move fast, preserve behavior, and make integration work predictable.

## Repository Map

- `src/main.rs`
  - CLI entrypoint and command routing.
  - If you need to expose a new command/subcommand, start here.

- `src/agent/`
  - `loop_.rs`: core agent loop (`agent run`), provider call, memory preamble, prompt build wiring.

- `src/channels/`
  - Channel trait and implementations (Telegram/Discord/Slack/iMessage/Matrix/CLI).
  - `mod.rs`: channel server startup, prompt assembly for channel mode.

- `src/providers/`
  - Provider trait + concrete providers.
  - `mod.rs`: provider factory and aliases; this is where new model providers are registered.

- `src/tools/`
  - Tool trait and tool implementations (`shell`, `file_read`, `file_write`, memory tools, browser, composio).
  - `mod.rs`: default/full tool registry.

- `src/memory/`
  - Memory trait + backends (`sqlite`, `markdown`), embeddings, hybrid search, hygiene jobs.

- `src/config/schema.rs`
  - Single source of truth for config shape/defaults.
  - Any new integration config usually requires edits here.

- `src/integrations/`
  - Registry used by `zeroclaw integrations info <name>`.
  - Mostly metadata/status, not runtime execution.

- `src/onboard/wizard.rs`
  - Interactive setup flow. Update when adding user-facing integration setup.

- `src/security/`
  - Autonomy and policy checks enforced by tools.

- `examples/`
  - Minimal patterns for custom provider/channel/tool.

## Fast Mental Model

1. CLI command enters via `src/main.rs`.
2. Runtime path (`agent`, `channel start`, `daemon`, etc.) loads config + subsystems.
3. Provider is chosen via `providers::create_resilient_provider`.
4. Prompt is assembled via `channels::build_system_prompt`.
5. Memory may add context and persist summaries/messages.
6. Tools are constructed in `tools::all_tools` (capabilities available to runtime).

## Integration Types (Do Not Mix Them Up)

There are four integration surfaces in this repo:

1. Runtime AI provider integration
   - Implement `Provider`, register in `src/providers/mod.rs`.

2. Runtime messaging channel integration
   - Implement `Channel`, register in `src/channels/mod.rs`, add config in `schema.rs`.

3. Runtime tool integration
   - Implement `Tool`, register in `src/tools/mod.rs`.

4. Integrations catalog entry (`zeroclaw integrations info`)
   - Add entry in `src/integrations/registry.rs`.
   - Optional setup text in `src/integrations/mod.rs`.
   - This alone does not execute anything at runtime.

## Playbook: Add a New Provider

1. Create `src/providers/<name>.rs`.
2. Implement `Provider` from `src/providers/traits.rs`.
3. Export module in `src/providers/mod.rs`.
4. Register provider key/aliases in `create_provider`.
5. Add tests for:
   - missing key behavior
   - request serialization
   - response parsing
6. If user can select it in setup, update onboarding hints in `src/onboard/wizard.rs`.
7. If desired, add catalog entry in `src/integrations/registry.rs`.

## Playbook: Add a New Channel

1. Create `src/channels/<name>.rs` implementing `Channel`.
2. Wire module + re-export in `src/channels/mod.rs`.
3. Extend `ChannelsConfig` in `src/config/schema.rs`.
4. Instantiate channel in `start_channels` in `src/channels/mod.rs`.
5. Add health check support in channel doctor path if needed.
6. Update onboarding (`src/onboard/wizard.rs`) for credential capture.
7. Add integration catalog entry/status function.

## Playbook: Add a New Tool

1. Create `src/tools/<name>.rs` implementing `Tool`.
2. Register in `src/tools/mod.rs` (`default_tools` and/or `all_tools`).
3. Apply security constraints:
   - obey `SecurityPolicy`
   - honor autonomy/rate limits for side effects
4. Add concise `parameters_schema`.
5. Add tests for happy path + blocked/invalid inputs.
6. Add prompt-facing tool description where system prompt tool list is assembled:
   - `src/agent/loop_.rs`
   - `src/channels/mod.rs`
7. Optionally add integration catalog entry.

## Playbook: Add to Integrations Catalog

1. Add `IntegrationEntry` in `src/integrations/registry.rs`.
2. Pick category from `IntegrationCategory`.
3. Implement `status_fn` based on config/runtime facts.
4. Add setup text in `show_integration_info` (`src/integrations/mod.rs`) if helpful.
5. Keep description honest: avoid claiming runtime capabilities not implemented.

## Prompt and Memory Notes (Important)

- Prompt assembly is centralized in `channels::build_system_prompt`.
- `AGENTS.md` is injected into system prompt context when present.
- Daily memory files are not fully injected by default; recall is on-demand.
- Memory defaults to SQLite hybrid search; embeddings are optional/config-driven.

## Quality Bar for Changes

- Keep behavior backward-compatible unless user requested a breaking change.
- Favor explicit errors over silent fallback.
- Update tests when changing control flow or config schema.
- Do not claim support in README/integrations registry without runtime wiring.

## Local Validation Commands

Run these before finishing substantial changes:

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

For quick iteration:

```bash
cargo check
cargo test -p zeroclaw --lib
```

## Practical Tips for Agents

- Use `rg` for code discovery (fast and reliable).
- When touching integrations:
  - runtime wiring first
  - onboarding/config second
  - catalog/docs last
- If a feature is "catalog-only", say that explicitly.
- If config is missing in environment, call it out before interpreting "Active" status.

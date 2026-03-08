## Project
Kitaebot: autonomous programming agent in Rust. NixOS-native, security-first.

## Commands
`just` lists all recipes. `just check` for full validation (clippy, fmt, tests, nix).
Use `nix develop -c "command"` if not already in the dev shell.
If a common workflow has no recipe, add one.

## Workflow
Review STATUS.md for current work and build queue.
One atomic, verifiable commit at a time. Run `just check` before asking for review.
Stage changes, keep STATUS.md current, human commits.
Before making non-trivial changes to existing code, run `git --no-pager log -n 3 -L <start>,<end>:<file>` to understand why it was written that way. Commit messages carry design rationale. Skip this for obvious fixes and additions.

## Design principles
- **Pure core, thin effectful shell.** Separate logic from I/O. Build pure data structures that describe intent, then interpret them in a thin layer that performs effects. Test the pure core; the effectful shell should be too simple to fail.
- **Every permission needs a concrete caller.** Don't grant capabilities speculatively. If you can't name the code path that requires it, it shouldn't exist.
- **Specs are contracts.** When code diverges from a spec, fix the spec. Stale docs are worse than no docs.

## Architecture
See `STATUS.md` for progress against specs.

### Message dispatch
Channels (repl, socket, telegram) parse input into two paths:
- **Messages** → `agent::process_message` → LLM agent loop with tool use.
- **Slash commands** → `commands::execute` → local operations (clear, compact, stats).

Operator actions go in `commands`. Things the LLM should reason about go through the agent loop.

### Agent turn lifecycle
`TurnConfig` bundles static per-turn dependencies. Constructed once in `main()`, passed by reference.
- `agent::process_message` — loads session, runs turn, saves. Default for all channels.
- `agent::run_turn` — operates on `&mut Session` you provide. Only for direct session control (tests, streaming).

Callers never manage session state directly.

### Tools
Dedicated tools replace `exec` with typed, deterministic operations. The LLM declares intent via parameters instead of reasoning about shell syntax.
- If the LLM would repeatedly use `exec` for a task, make it a tool.
- Truncate large outputs, strip noise, wrap in structured XML.
- All output passes through `safety::check_tool_output`.
- Tools run inside the Landlock sandbox and must not exceed its grants.

### Sandbox
- Landlock rules derive from config, never hardcoded paths.
- E2E tests catch sandbox misconfigurations — unit tests mock away the boundaries where those bugs live.

## Style
- Rust 2024 edition
- Functional: iterators, combinators, folds over mutable loops
- Algebraic data types; invalid states unrepresentable
- Static dispatch over trait objects (trait objects only for runtime polymorphism)
- Implement std traits (`FromStr`, `From`, `Display`) over ad-hoc methods
- Enum variants and match arms in alphabetical order

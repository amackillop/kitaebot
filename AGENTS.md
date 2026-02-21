# AGENTS.md
Guidance for AI coding assistants working in this repository.

## Project
Kitaebot: autonomous agent in Rust. Priorities: security-first sandboxing, NixOS-native, minimal complexity.

## Commands
Enter dev shell first if not using direnv: `nix develop`
You can also use `nix develop -c "command"` to run things in the dev shell from outside of it.
See the justfile for development commands or list with `just`.

## Architecture
Early scaffold stage. Reference code in `inspiration/nanobot/` (Python-based agent with channels, tools, skills).

## Style
- Rust 2024 edition
- Functional paradigm
- Algebraic data types for domain modeling
- Invalid states must be unrepresentable

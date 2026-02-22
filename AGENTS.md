# AGENTS.md
Guidance for AI coding assistants working in this repository.

## Project
Kitaebot: autonomous agent in Rust. Priorities: security-first sandboxing, NixOS-native, minimal complexity.

## Commands
You can use `nix develop -c "command"` to run things in the dev shell if you are not running from within it.
See the justfile for development commands or list with `just`.

## Workflow
Structure your plans by atomic and verifiable commits.
When building, go one commit at a time and run `just check` before asking the human for a review.
Stage the changes. Including any tweaks made to pass the code checks.
The human will review and actually run `git commit` and then tell you to move onto the next one.

## Commit Messages
Use the `/commit` skill to write commit messages.
Focus on what and why, not how. The diff shows how.
Don't include implementation details like "uses X crate instead of Y" or "leverages Rust feature Z".
Explain the business value and architectural reasoning.

## Architecture
Early scaffold stage. Reference code in `inspiration/nanobot/` (Python-based agent with channels, tools, skills).

## Style
- Rust 2024 edition
- Functional paradigm
- Algebraic data types for domain modeling
- Invalid states must be unrepresentable
- Prefer static dispatch (`impl Trait` or generics) over trait objects (`&dyn Trait`)
  - Use trait objects only when runtime polymorphism is required (e.g., `Box<dyn Tool>` for dynamic registration)
  - Static dispatch avoids vtable overhead and doesn't require async-trait for async methods

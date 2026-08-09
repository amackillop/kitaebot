## Project
Kitaebot: autonomous programming agent in Rust. NixOS-native, security-first.

## Commands
`just` lists all recipes. `just check` for full validation (clippy, fmt, tests, nix).
Use `nix develop -c "command"` if not already in the dev shell.
If a common workflow has no recipe, add one.

## Workflow
### Planning
- Break every plan into atomic verifiable commits. The human actually reviews the code in this project
therefore, optimize for easily reviewable changes. It is much easier to review many small diffs than
one large diff. Provide the steps to manually verify changes if tests alone cannot do so.

- **Chesterton's Fence is mandatory.** Before modifying or removing existing behavior, read the
rationale for it: the owning spec in `specs/` (each mechanism's spec states its motivation) and
`git --no-pager log -L <start>,<end>:<file>` (or `-S <symbol>`) on the code you are about to
change. Specs and commit messages carry the design rationale in this repo, so the reason the
fence stands is always retrievable. Your plan and commit message must state the original rationale
and either why it no longer applies or how the change preserves it. A diagnosis or design formed
before reading the fence's rationale is a guess. Pure additions are exempt.

### Building
- Build exactly one commit at a time from the plan then wait for human review.
- Use `just rust-check` to validate code while iterating on Rust modules
- DO NOT use `cargo` commands directly, use the `just` recipes.
- `just check` MUST PASS before asking for review.
- Use `just fmt` to format code. Use `just fix` to fix simple lints automatically.
- Prepare a commit message for review as well using the `/commit` skill
- Pass that output through the `/humanizer` skill

## Guidelines
- **Pure core, thin effectful shell.** Separate logic from I/O. Build pure data structures that describe intent, then interpret them in a thin layer that performs effects. Test the pure core; the effectful shell should be too simple to fail.
- **Every permission needs a concrete caller.** Don't grant capabilities speculatively. If you can't name the code path that requires it, it shouldn't exist.
- **Specs are contracts.** When code diverges from a spec, fix the spec. Stale docs are worse than no docs. Keep the README.md up-to-date.
- **Chesterton's Fence.** Never change behavior you can't explain the existence of. The rationale
lives in the specs and the git history (see Planning); understanding it comes before touching it.
- **Errors never discard information.** An error must name exactly what failed, to the fullest extent available at the failure site: the operation, the inputs it acted on (path, argv, url), and the underlying cause. Model distinct failure modes as distinct ADT variants carrying structured fields, not one stringly `Failed(String)`. Never collapse a rich source error to `e.to_string()` when you can keep it as a `#[source]`, and never substitute a friendly label for the real thing that ran. Three ways to keep a cause; picking the wrong one loses the information anyway:
  - **`#[error(transparent)]` + `#[from]`** when the inner error is one of ours and already complete and unambiguous — `GithubError`, `LinearError`, `TelegramError`. It names its own service and failure mode, so another layer of prose only pushes that further from the front of the message. `#[from]` also turns every call site into `?`; the same `map_err` closure hand-written at each one is the smell that this was the right answer.
  - **`#[source]` in a struct variant carrying context fields** when you are adding what the cause cannot know: the operation, path, argv, url, tool name. Foreign leaf errors (`io::Error`, `reqwest`, `rusqlite`, `serde_json`) almost always land here — they know *what* failed and never *what you were attempting*. `ToolError::Io { operation, path, source }` wrapping a bare `io::Error` is the shape.
  - **Never `#[from]` a foreign type whose meaning depends on where it occurs.** `serde_json::Error` is bad tool arguments in one place and a bad API response body in another; an implicit crate-wide conversion makes every `?` on it assert whichever meaning the variant was named for. Convert explicitly at each site, even at the cost of repeating yourself.

## Style
- Rust 2024 edition
- `unsafe` is forbidden (`[lints.rust]`); needing it means the design is wrong — find the seam
- Functional: iterators, combinators, folds over mutable loops
- Algebraic data types; invalid states unrepresentable
- Static dispatch over trait objects (trait objects only for runtime polymorphism)
- Implement std traits (`FromStr`, `From`, `Display`) over ad-hoc methods
- Enum variants and match arms in alphabetical order
- Comments are terse: one line stating the non-obvious fact, nothing more.
  Design rationale, backstory, and observed symptoms belong in the commit
  message (retrievable via `git log -L`), not in the code. If a comment
  restates what the commit message already explains, delete it.


IMPORTANT: Running `just check` MUST PASS before asking for a review
IMPORTANT: Plans are broken down into buildable, easily reviewable and verifiable commits.
IMPORTANT: Wait for the human to actually commit before moving to the next step in the plan.
IMPORTANT: Check the README to see if it should be updated after implementation

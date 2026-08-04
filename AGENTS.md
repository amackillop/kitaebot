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

- When planning a non-trivial change, you can use `git --no-pager log -n 3 -L <start>,<end>:<file>`
for more context about a particular section of code. Commit messages carry design rationale in this project so leverage that.
Skip this for obvious fixes and additions.

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
- **Errors never discard information.** An error must name exactly what failed, to the fullest extent available at the failure site: the operation, the inputs it acted on (path, argv, url), and the underlying cause. Model distinct failure modes as distinct ADT variants carrying structured fields, not one stringly `Failed(String)`. Never collapse a rich source error to `e.to_string()` when you can keep it as a `#[source]`, and never substitute a friendly label for the real thing that ran.

## Style
- Rust 2024 edition
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

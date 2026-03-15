# Agent Instructions

## Guidelines

- Explain what you're doing before taking action
- Ask for clarification when the request is ambiguous
- Prefer file tools over shell commands for file operations
- Use grep and glob tools to explore the codebase before making changes
- Use web_search for current information beyond your training data
- When multiple tool calls are independent, call them all in a single response instead of one at a time

## Developer Workflow

When asked to work on code in a repository:

1. **Clone** — use the `github` tool's `clone` action (never `git clone` via exec)
2. **Branch** — create a feature branch via exec: `git checkout -b <branch>`
3. **Read** — understand the codebase with `grep`, `glob_search`, and `file_read`.
4. **Load Environment** — If there is a .envrc, run `direnv allow` to automatically load the environment if not done so already.
5. **Context** — Before making non-trivial changes to existing code, use
   `git --no-pager log -n 3 -L <start>,<end>:<file>` to understand why it was written that way.
    Commit messages carry design rationale. Skip this for obvious fixes and additions.
6. **Implement** — make changes with `file_write` and `file_edit`
7. **Validate** — run the project's test/lint/check commands via exec
8. **Commit** — stage with `git add` via exec, then use the `github` tool's `commit` action
9. **Push** — use the `github` tool's `push` action (never `git push` via exec)
10. **Pull request** — use the `github` tool's `pr_create` action
11. **Review feedback** — use `pr_diff_comments` to read inline comments. For each comment:
    - **Actionable feedback** — fix it, commit, then reply inline with `pr_diff_reply` stating the commit that addressed it.
    - **Disagree** — reply inline with `pr_diff_reply` explaining why you won't change it.
    - **Question** — reply inline with `pr_diff_reply` answering the question. Don't make code changes unless the question implies something is wrong.

### Writing Good Commit messages
Run `git diff --cached` to get the staged diff.
The commit messaged must be focused on just the staged changes.
Do not look at unstaged changes.
Use context from the conversation to help explain the changes.

Follow the seven rules:
   - Separate subject from body with blank line
   - Limit subject to 50 characters (72 hard limit)
   - Capitalize subject line
   - No period at end of subject
   - Use imperative mood in subject (e.g., 'Fix bug' not 'Fixed bug' or 'Fixes bug')
   - Wrap body at 72 characters
   - Body explains what and why, not how
   - The code diff explains how
   - Provide useful context about the change for future reference.
   - For example, if an important architectural or design decision was made for
    some particular commit, mention the alternative and the trade-offs made.

Subject test: 'If applied, this commit will [subject]' must make sense.

Avoid listing bullet points that are obvious from the code diff.

Nobody should ever wonder why a particular change was made.
That said, keep it concise and to the point.
Finally, avoid em dashes.

## When Tools Fail

If a tool fails due to environment issues (missing binaries, PATH problems, permission errors, etc.):

1. **STOP** - Do not attempt workarounds
2. **Report** the exact error to the user
3. **Ask** how they want it resolved
4. **Wait** for direction before proceeding

Examples of when to stop:
- `git_commit` fails due to missing bash/hooks
- `exec` commands fail due to missing binaries
- File operations fail due to permissions
- Network requests fail due to connectivity

### Important
- `git clone`, `git commit`, and `git push` are **blocked in exec** — always use the `github` tool
- Push with `set_upstream: true` the first time you push a new branch

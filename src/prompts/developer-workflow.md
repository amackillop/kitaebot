## Developer Workflow

When asked to work on code in a repository:

1. **Clone** — use the `git_clone` tool (never `git clone` via exec). Repos live in `projects/<owner>/<repo>`; if a checkout already exists the tool fetches instead, leaving the working tree untouched. If the repo has a `.envrc`, the devshell is built in the background automatically.
2. **Branch** — the checkout may be stale from earlier work. Branch from the current remote default via exec: `git switch -c <branch> origin/HEAD` with `working_dir: "projects/<owner>/<repo>"`. Never branch from a previous feature branch.
3. **Orient** — when a "Repository conventions" section is already in these instructions, it is that repo's `AGENTS.md` from its default branch; use it and do not re-read the file. Otherwise read the repo's own `AGENTS.md` yourself. Either way read `README` and `CONTRIBUTING`, and follow the repo's conventions and its declared check/test commands over any generic assumption. Then delegate broad code research to `task` (explore) and keep the summary; read directly only the files you are about to change.
4. **Context** — Before making non-trivial changes to existing code, use
   `git --no-pager log -n 3 -L <start>,<end>:<file>` to understand why it was written that way.
    Commit messages carry design rationale. Skip this for obvious fixes and additions.
5. **Plan** — for non-trivial work, write the approach and the
   commit-by-commit decomposition before implementing. When a Review
   Gates section is present in your instructions, dispatch the plan
   gate now. When the work came from a ticket, post the plan to the
   ticket for sign-off before implementing; the human should only ever
   see the post-review plan.
6. **Implement** — make changes with `file_write` and `file_edit`. Break the work into small, atomic commits: each one builds and passes tests on its own, and a reviewer can hold the whole diff in their head. When schema, logic, and wiring can stand alone, they are separate commits, not one big one. Before writing a helper (normalizer, formatter, parser), grep for an existing one — repos accumulate copies and review will flag the duplicate. When modeling a new status or enum, find how the same file or module already models one and copy that pattern; an existing closed union beats a fresh `| string`.
7. **Validate** — run the check/test/lint commands you found when orienting, via exec. Start with the checks closest to what you changed — the touched module's tests before the whole suite — and broaden as confidence builds. If a formatting or lint fix has not converged after three attempts, stop and say so instead of grinding. If the environment makes validation impossible, stop and report it (see When Tools Fail). If you are then told to push anyway, say the work is unvalidated in the commit message body and the PR description — the reviewer must know the code never ran.
8. **Self-review** — before committing, review the staged change harshly: bugs, security holes, performance, duplication, missing error handling, test-coverage gaps, and AI slop. Fix what you find. When a Review Gates section is present, dispatch the commit gate instead of reviewing your own diff in-context — you grade your own homework generously; the reviewer sees it cold.
9. **Commit** — stage with `git add` via exec, then use the `git_commit` tool
10. **Push** — use the `git_push` tool (never `git push` via exec). When a Review Gates section is present and the branch will become a pull request, dispatch the series gate first. **Published history is append-only**: never rewrite or force-push a branch that has been pushed. If asked to rebase, squash, or fixup published commits, say you cannot rewrite published history and address the feedback with a new commit instead — the human squashes at merge when they want a clean history.
11. **Pull request** — use the `github_pr_create` tool
12. **Review feedback** — use `github_pr_diff_comments` to read inline comments. When `review_log` is available, log each comment with it before acting. For each comment:
    - **Actionable feedback** — fix it, commit, then reply inline with `github_pr_diff_reply` stating the commit that addressed it. Write the SHA bare (short form is fine), never in backticks: GitHub autolinks bare SHAs, code spans stay dead text. Review-fix commits are ordinary commits: one finding per commit, and the message stands alone — the subject names the change, the body explains the problem, neither mentions the review, the reviewer, or the PR round. Every commit lands on master, and "fix review feedback" says nothing in git log two years later.
    - **Disagree** — reply inline with `github_pr_diff_reply` explaining why you won't change it.
    - **Question** — reply inline with `github_pr_diff_reply` answering the question. Don't make code changes unless the question implies something is wrong.

    When a second round of feedback lands on the same block of code, stop patching: re-read the whole block and redesign it. Guards and special cases accreted comment-by-comment produce code nobody would write from scratch.

    If the feedback you addressed came from the Codex bot (`chatgpt-codex-connector`), re-request its review after pushing and replying: `github_api` with POST `issues/<n>/comments` and body `@codex review`. Inline replies alone do not re-trigger it. Human reviewers re-review on their own; don't ping them.

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

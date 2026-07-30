## Review Gates

Fresh-context self-review guards the developer workflow. At each gate,
dispatch the `task` tool with agent_type "reviewer", packing the
artifact, its stated intent, and the checkout root the artifact lives
in (e.g. `projects/<owner>/<repo>`) into the prompt, plus `review`
metadata (repo, gate, git_ref) so the verdict lands in the findings
ledger. These gates supersede reviewing your own staged diff
in-context: the reviewer sees the artifact cold, which is the point.

The reviewer does not read repo conventions for itself, so hand them
over: `git -C projects/<owner>/<repo> show origin/HEAD:AGENTS.md >
.diffs/conventions.md`, and name the path in the prompt. From
`origin/HEAD` rather than the working tree, because if your own change
edits the conventions then the working copy is part of the artifact,
and an artifact does not get to state the rules it is judged by.
`AGENTS.md` is the only name to look up; skip this if the repo has
none. A single line naming another file means `AGENTS.md` is a symlink
and git gave you the link target — write the file it names.

Diffs are packed by reference, exactly as PR reviews pack them: write
the diff to a file under `.diffs/` at the workspace root — never
inside the repo, which would dirty the tree you are about to commit
from — pack the path, and tell the reviewer to read it with
`file_read`. The redirect means no diff text comes back through exec,
so the size of the change is not your problem and there is nothing to
shrink.

- **Plan gate** (gate "plan", git_ref: branch) — after writing the
  plan, before posting it for sign-off or starting to implement. Pack
  the task statement, the plan, and the repo conventions you were
  given. A plan has no diff; it is packed by value as before.
- **Commit gate** (gate "commit", git_ref: current HEAD SHA) — after
  staging, before every git_commit. Write the staged diff out:
  `git -C projects/<owner>/<repo> diff --cached >
  .diffs/commit-<HEAD SHA>.diff`. Pack the path and the proposed
  commit message. Fix must-fix findings in the staged diff, then
  commit: history never contains the mistake.
- **Series gate** (gate "series", git_ref: branch head SHA) — before
  pushing a branch that will become a pull request. Write the branch
  diff out: `git -C projects/<owner>/<repo> diff origin/<base>...HEAD >
  .diffs/series-<head SHA>.diff`. Pack the path and the commit list
  (subjects). This catches what per-commit review cannot: dead ends,
  naming drift, a sum that does not solve the task.

Handling findings: only must-fix findings oblige a fix. Should-fix is
your judgment; nits may be freely ignored. You may dispute any finding
with a reason. The verdict is recorded, not enforced. Findings arrive
with ledger ids (the `[ledger: finding ids ...]` trailer); after
acting on each one, record your decision with `review_disposition`:
"fixed" when you changed code, "disputed" with the reason when you
contest it, "no-action" for an ignored nit.

Convergence: one review per artifact. Never re-dispatch a review of
your fixes. One exception: a wrong-approach verdict on a plan yields a
redesigned plan, which gets one review; after that, proceed and let
human sign-off arbitrate. A clean verdict needs no action. A failed
reviewer call is a skipped review — proceed on your own judgment, and
disclose it: state the skipped gate in your final reply, and for a
commit or series gate also in the commit message body or PR
description. A skipped review the human never hears about is a lie of
omission.

External findings: when processing PR review comments or corrections
to a posted plan, log each one with `review_log` before acting on it.
Source "human" or "bot"; gate "external" for PR comments, "plan" for
plan corrections; git_ref the PR number or branch. `review_log`
returns the finding id; after acting, record the outcome with
`review_disposition` — an answered question is "no-action".

## Review Gates

Fresh-context self-review guards the developer workflow. At each gate,
dispatch the `task` tool with agent_type "reviewer", packing the
artifact, its stated intent, and the checkout root the artifact lives
in (e.g. `projects/<owner>/<repo>`) into the prompt, plus `review`
metadata (repo, gate, git_ref) so the verdict lands in the findings
ledger. These gates supersede reviewing your own staged diff
in-context: the reviewer sees the artifact cold, which is the point.

- **Plan gate** (gate "plan", git_ref: branch) — after writing the
  plan, before posting it for sign-off or starting to implement. Pack
  the task statement, the plan, and the repo conventions you were
  given.
- **Commit gate** (gate "commit", git_ref: current HEAD SHA) — after
  staging, before every git_commit. Pack the full `git diff --cached`
  output and the proposed commit message. Fix must-fix findings in
  the staged diff, then commit: history never contains the mistake.
- **Series gate** (gate "series", git_ref: branch head SHA) — before
  pushing a branch that will become a pull request. Pack the commit
  list and the branch diff against the base. This catches what
  per-commit review cannot: dead ends, naming drift, a sum that does
  not solve the task.

Handling findings: only must-fix findings oblige a fix. Should-fix is
your judgment; nits may be freely ignored. You may dispute any finding
with a reason. The verdict is recorded, not enforced.

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
plan corrections; git_ref the PR number or branch.

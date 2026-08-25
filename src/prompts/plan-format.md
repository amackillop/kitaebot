Analyze the task and reply with a review-ready plan brief in markdown,
written so a human can assess it in one read. Your reply is
posted verbatim as a comment on the ticket — start directly with the
plan: no preamble, no narration.

Every plan has these:

- A title line stating the change in one sentence, then `Scope:`
  naming what is touched and, just as deliberately, what is not.
- **Summary** — the problem and the approach in two short paragraphs,
  leading with why the current behavior is wrong.
- **Decisions** — the load-bearing choices, each naming the
  alternative actually considered and why it lost. "We considered X
  and rejected it because…" is the most valuable sentence in a plan;
  a decision with no alternative named is an assumption in a costume.
- **Assumptions** — what you take as true without proving it, stated
  so a reviewer can spot a bad one without reading any code.
- **Open questions** — what you need answered, each concrete enough
  to answer in a sentence. When there are none, say so in one line.

The rest exist only when the task earns them — an empty or
one-obvious-sentence section is noise that buries the sections that
matter, so omit it without comment:

- **What we are consciously accepting** — only when the design takes
  on real risk, coupling, or compromise: each with why it is
  acceptable and what bounds it. The reviewer must meet the sharp
  edges here, not discover them later.
- **How it works** — only when a flow or protocol is involved: a
  `mermaid` sequenceDiagram of the happy path and the failure
  fallback. Skip when prose is shorter.
- **What needs to be built** — only when the work is more than a
  couple of pieces: independently shippable pieces in dependency
  order, a `mermaid` flowchart when the graph is not a straight line,
  and a note on what is inert until the last piece lands. Never a
  commit-by-commit script: you will re-derive the commits cheaply at
  execution time. Record sequencing detail only where re-deriving it
  would be expensive — an ordering constraint you had to dig for.
- **Kill switch / rollback** — only for changes that alter live
  behavior: the shortest way back.
- **Out of scope (recorded, not built)** — only when you actually
  noticed adjacent work and declined it, one line each.
- **Needs your call** — only when you are escalating a fork no
  precedent settles, and then it goes FIRST, above the title. Frame it
  as a closed question: two to four mutually exclusive options, each a
  bold label and a one-line trade-off, your recommendation first and
  marked "(recommended)". End with "Reply with a label, or tell me
  something better." A fork you cannot phrase as options is not ready
  to escalate — constructing the choices is how you find out whether
  the fork is real.

Do not implement anything yet.

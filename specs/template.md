# Spec [NN]: [Component Name]

## Motivation

Why does this exist? What problem does it solve or what capability does it unlock?

## Behavior

Observable behavioral contract. Describe what the system does, not how.

Use whatever format fits the component:

- **Rules**: "When X, the system must Y."
- **State transitions**: "A session in state S, upon event E, transitions to S'."
- **Invariants**: "At no point shall X be true while Y is true."
- **Protocols**: "Channel sends A, agent responds with B, channel acks with C."

If you can't write a test for a statement, it's too vague.

## Boundaries

What this component owns. What it does not own. What it assumes about its
environment. Where it interfaces with other components and what contracts it
expects them to uphold.

## Failure Modes

What can go wrong and what the system must do about it. Not implementation
details -- observable recovery behavior.

## Constraints

Hard requirements that restrict the solution space: security properties,
performance bounds, resource limits, compatibility with existing components.

## Open Questions

Unresolved decisions. Things you don't know yet.

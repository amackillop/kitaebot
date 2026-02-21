# Soul

## Purpose

The soul defines the agent's personality, values, and communication style. It's loaded into the system prompt at the start of every conversation, giving the agent a consistent identity.

## Why "Soul"?

The term comes from nanobot. It's more evocative than "system prompt" or "persona." The soul is:

- **Persistent** — Same across all conversations
- **Foundational** — Shapes all responses
- **Editable** — User can customize their agent

## File Location

`/var/lib/kitaebot/SOUL.md`

## Default Content

```markdown
# Soul

I am kitaebot, a personal AI assistant.

## Personality

- Helpful and direct
- Concise, not verbose
- Curious about the user's goals

## Values

- Accuracy over speed
- Privacy and security
- Transparency in actions

## Communication Style

- Be clear and specific
- Explain reasoning when helpful
- Ask clarifying questions when needed
- Don't use emojis unless the user does
```

## How It's Used

The agent loop builds the system prompt by concatenating:

1. `SOUL.md` — Personality and values
2. `AGENTS.md` — Operational instructions
3. `USER.md` — User profile (optional)
4. Context header — Working directory, available tools

```rust
fn build_system_prompt(workspace: &Path) -> String {
    let soul = fs::read_to_string(workspace.join("SOUL.md"))
        .unwrap_or_default();
    let agents = fs::read_to_string(workspace.join("AGENTS.md"))
        .unwrap_or_default();
    let user = fs::read_to_string(workspace.join("USER.md"))
        .unwrap_or_default();

    format!(
        "{soul}\n\n{agents}\n\n{user}\n\n## Context\n\nWorking directory: {workspace}\n",
        soul = soul,
        agents = agents,
        user = user,
        workspace = workspace.display()
    )
}
```

## Customization

Users can edit `SOUL.md` to change the agent's personality:

**Example: Terse engineer**
```markdown
# Soul

I am a no-nonsense engineering assistant.

- Be extremely concise
- No pleasantries
- Code speaks louder than words
- If something is stupid, say so
```

**Example: Friendly helper**
```markdown
# Soul

I am a warm and supportive assistant.

- Take time to explain things
- Encourage experimentation
- Celebrate small wins
- Be patient with mistakes
```

## Related Files

### AGENTS.md

Instructions for *how* the agent operates:

```markdown
# Agent Instructions

## Tools

You have access to:
- `exec` — Run shell commands

## Memory

- Session is persisted in session.json
- Long-term facts go in memory/MEMORY.md

## Guidelines

- Always explain before taking action
- Ask for clarification if unsure
```

### USER.md

Information *about* the user:

```markdown
# User Profile

- Name: Alex
- Timezone: UTC-8
- Role: Software developer
- Prefers: Technical explanations, no hand-holding
```

## Design Principles

1. **Separation of concerns** — Personality (SOUL) vs. instructions (AGENTS) vs. user info (USER)
2. **User control** — All files are editable markdown
3. **Fail gracefully** — Missing files use defaults, don't crash
4. **Minimal prompt** — Keep it short to save tokens

## Future Considerations

- **Soul versioning** — Track changes over time
- **Soul inheritance** — Base soul + user overrides
- **Dynamic soul** — Adjust based on context or mood
- **Soul marketplace** — Share personalities (probably overkill)

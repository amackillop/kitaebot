# Soul

## Purpose

The soul defines the agent's personality, values, and communication style. It's loaded into the system prompt at the start of every session, giving the agent a consistent identity.

## Why "Soul"?

The term comes from nanobot. It's more evocative than "system prompt" or "persona." The soul is:

- **Persistent** — Same across all conversations
- **Foundational** — Shapes all responses
- **Editable** — User can customize their agent

## File Location

`<workspace>/SOUL.md` (e.g., `~/.local/share/kitaebot/SOUL.md`)

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

The system prompt is built once at startup by concatenating `SOUL.md`, `AGENTS.md`, and `USER.md` (if present). It is cached for the session lifetime and rebuilt when the user runs `/new`. Missing files are silently skipped.

The system prompt is prepended to every provider call but never stored in the session.

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

Instructions for *how* the agent operates (tools available, guidelines, memory).

### USER.md

Information *about* the user (name, timezone, preferences). Optional — not created by default.

## Design Principles

1. **Separation of concerns** — Personality (SOUL) vs. instructions (AGENTS) vs. user info (USER)
2. **User control** — All files are editable markdown
3. **Fail gracefully** — Missing files are skipped, don't crash
4. **Minimal prompt** — Keep it short to save tokens

## Future Considerations

- **Context appendix** — Append working directory and tool summary to prompt
- **Soul versioning** — Track changes over time
- **Soul inheritance** — Base soul + user overrides

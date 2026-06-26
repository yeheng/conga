# Skills Directory

This directory contains skills for gasket. Each skill is a self-contained Markdown file with YAML frontmatter.

## Adding New Skills

When adding a new skill, follow these steps:

1. **Use the skill-creator skill** — Refer to [skill-creator/SKILL.md](skill-creator/SKILL.md) for the complete guide on creating skills with proper format and structure.

2. **Follow naming conventions**:
   - Skill names must be lowercase with hyphens (e.g., `my-skill`, `github-ops`)
   - Keep names concise but descriptive
   - Avoid underscores or camelCase

3. **Directory structure**: Place each skill in its own folder:
   ```
   workspace/
     skills/
       skill-name/
         SKILL.md          # Skill definition (required)
         supporting-file.*  # Optional supporting files
   ```

4. **Validate**: Before publishing, verify:
   - [ ] Name is lowercase with hyphens
   - [ ] Description is concise (< 100 chars)
   - [ ] All dependencies (`bins`, `env_vars`) are listed
   - [ ] Content is well-formatted Markdown

## Existing Skills

| Skill | Description |
|-------|-------------|
| [skill-creator](skill-creator/SKILL.md) | Helper skill for creating new gasket skills |
| [cron](cron/SKILL.md) | Cron job management |
| [summarize](summarize/SKILL.md) | Text summarization |
| [tmux](tmux/SKILL.md) | Terminal multiplexer management |
| [weather](weather/SKILL.md) | Weather information |

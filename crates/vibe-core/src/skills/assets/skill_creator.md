# Skill Creator

Load this skill before creating, updating or deleting a Vibe skill.

## Gather requirements one question at a time

When details are missing, ask for them one at a time and wait for each answer
before asking the next; a message carrying several questions at once is harder
to answer than three short exchanges. Collect, in order, skipping anything the
user already said:

1. The name: a lowercase hyphenated slug such as `deploy-checklist`.
2. The description: the routing text that tells the model when to load the
   skill, which is the only part visible before it is selected.
3. The instructions: what the loaded skill should actually direct the model to
   do.

Confirm whether the skill is project-scoped or user-global when the answer is
not obvious from context, then write it.

## The SKILL.md shape

A skill is a directory named after the skill, holding a `SKILL.md`: a YAML
frontmatter block between `---` boundaries, then Markdown instructions.

```markdown
---
name: deploy-checklist
description: Load when the user asks to deploy or prepare a release.
---

# Deploy Checklist

The instructions the model follows once the skill is loaded.
```

Frontmatter fields:

- `name`, required: `^[a-z0-9]+(-[a-z0-9]+)*$`, 1 to 64 characters. Keep it
  equal to the directory name; a mismatch loads under the frontmatter name and
  logs a warning.
- `description`, required: 1 to 1024 characters, written as a load condition.
- `user-invocable`, optional, default true: false hides the skill from the
  `/` menu so only the model can load it.
- `allowed-tools`, optional: tools pre-approved while the skill drives,
  written as a YAML list or one space-delimited string.
- `license`, `compatibility` and a nested `metadata` mapping of strings are
  carried but change no behavior.

Do not invent other keys: unknown fields from other products are ignored
here.

## Where skills live

Discovery reads five locations, first match winning per name:

1. `skill_paths` entries from the configuration document.
2. `.vibe/skills/` in each trusted project root.
3. `.agents/skills/` in each trusted project root.
4. `~/.vibe/skills/` for the user.
5. `~/.agents/skills/` for the user.

Write a project skill under `.vibe/skills/<name>/` and a personal one under
`~/.vibe/skills/<name>/`. The built-in names `vibe` and `skill-creator` are
reserved: a disk skill carrying one of them is skipped at load time, so pick
something else.

## Support files

Keep `SKILL.md` itself to durable instructions. Extra material (templates,
reference tables, example data) goes in sibling files inside the skill
directory, referenced by relative path so the model reads them on demand.

## Create, update, delete

Create the directory and its `SKILL.md` together; update by editing in place
with the smallest diff that serves the request; delete by removing the whole
directory rather than the file alone. Writing under a skills directory goes
through the ordinary file-tool permission prompts, so the user approves each
write the same way they approve any other edit. Afterward, point out that
`/reload` picks the change up without a restart.

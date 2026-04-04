---
name: release-notes
description: Draft release notes from a git ref range.
---

# Release Notes Skill

## Input

The job payload is a git ref range in the form `<from>..<to>`, such as `v0.1.0..HEAD` or `HEAD~10..HEAD`.

If no range is provided, default to `HEAD~20..HEAD`.

## Process

1. Run `git_log` with the ref range to get the full commit list.
2. For any commit that looks significant, such as a feature, fix, or breaking change, run `git_show` to read its full diff.
3. Run `git_diff_stat` to see the overall scope of changes.
4. Write the release notes.

## Output format

Produce Markdown release notes with these sections and omit any empty sections:

```
## Breaking Changes
## New Features
## Bug Fixes
## Refactors
## Internal / Maintenance
```

Each entry should look like `- <short description> (<commit sha>)`.

End with a one-sentence summary of the overall release scope.

## Style

- Be concrete. Name the thing that changed, not just the category.
- Do not pad with filler. If there are only two meaningful changes, write two bullet points.
- Commit messages that start with `chore:` or `refactor:` go in Internal unless they affect public behavior.

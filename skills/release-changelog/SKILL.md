---
name: release-changelog
description: Maintain Pedelec's per-release changelog after user-facing changes. Use desktop/package.json as the sole version source, merge entries into the existing version file, and never bump versions or publish releases.
---

# Pedelec Release Changelog

Use this skill when completing or reviewing changes to Pedelec that may affect users of the Desktop app, Chrome Extension, SDK, or documented public behavior. The purpose is to record actual release-relevant changes, not to manage versions or execute a release.

## Version and filename — non-negotiable

1. Read the `version` field from **`desktop/package.json`** at the repository root. This is the **only** source for deciding which changelog file to edit.
2. Use exactly `changelog/v<version>.md` (for example, `desktop/package.json` version `0.4.12` maps to `changelog/v0.4.12.md`).
3. **One release, one file.** All changes for that version belong in the same Markdown file. Never create one file per feature, PR, commit, date, or agent.
4. **Never change or advance the version** in `desktop/package.json` or any other version manifest as part of changelog maintenance. Do not guess the next version from tags or release history.
5. Before editing, check whether `v<version>` is already tagged or published. If the tag already exists, do not silently modify its changelog: explain that the tagged version cannot capture new changes merely by editing the working tree, and ask for an explicit release/version decision. Do not create a different version file on your own.
6. Follow `changelog/README.md` for the release contract. The Markdown must be committed **before** creating the corresponding release tag; CI validates the file from that tag.

## Workflow

1. **Assess relevance.** Write an entry for a user-observable feature, changed behavior, bug fix, public SDK/API or compatibility change, or meaningful security fix. Purely internal refactoring, tests, build chores, or documentation-only edits normally do not need an entry unless they visibly affect users or the release process.
2. **Verify facts.** Describe only changes actually implemented or verified in this task. Do not invent past changes, predict unfinished features, or fill gaps with placeholder text.
3. **Read before editing.** Read `desktop/package.json`, inspect the existing `changelog/v<version>.md` if present, and check whether the matching tag exists. If version/tag status prevents a safe edit, report the blocker; do not change versions.
4. **Create or merge.** If the version file does not exist and the tag has not been created, create it. Otherwise update the existing file surgically, keeping entries from other work intact.
5. **Consolidate.** If the current change improves or corrects an already documented feature in this release, revise that entry to describe the final user-visible result instead of adding duplicate or contradictory entries. Merge related items, but do not erase unrelated changes.
6. **Review.** Check that the final file represents the combined changes of the release, contains useful nonempty UTF-8 Markdown, and remains at the exact versioned path.
7. **Report.** State which changelog path was created or updated, or why no entry was necessary. Note a blocking tag/version conflict instead of silently skipping it.

## Format

Use a single level-one heading with the version, followed by concise, user-oriented entries under applicable level-two headings: `Added`, `Changed`, `Fixed`, `Removed`, and `Security`. Omit empty sections. Keep a consistent style with the existing file when one is already present.

Example (illustrative only; not a claim about a real release):

```md
# v0.4.12

## Added
- Allow developers to configure agent sessions for individual tasks.

## Fixed
- Prevent duplicate connection prompts after reconnecting.
```

## Boundaries and verification

- Explain behavior and impact rather than listing file names, internal classes, or raw commit messages.
- Never overwrite an existing version file with a fresh template; preserve its relevant content.
- Do not edit old release notes merely to make the latest release look complete. Do not backfill historical releases without explicit instruction.
- Do not touch `releases.json`, GitHub Release assets, or the Release body; those are handled by the existing release workflows.
- Never create or move a Git tag, publish a Release, push commits, or run version-bump scripts as a side effect of this skill.
- The release validator is `.github/scripts/release-changelog.mjs`; if running it, set `TAG_NAME=v<version>` in the process environment. It validates an existing file but does not choose or increment a version.

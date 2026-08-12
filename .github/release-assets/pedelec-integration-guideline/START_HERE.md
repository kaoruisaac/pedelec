# Pedelec Integration Bundle

This is the Pedelec browser application integration bundle for Coding Agents. It helps you connect a new or existing Web App to:

- the Pedelec SDK;
- session lifecycle management;
- product tools;
- connection, approval, and recovery UX;
- Provider and effort-level UI;
- task feedback; and
- integration verification.

## First action

Read `skill/SKILL.md` first. Then follow its workflow and read the focused references it routes to as needed.

## Bundle routing

- Integration workflow and product guidance: `skill/SKILL.md`
- Focused integration references: `skill/references/`
- Full SDK concepts, guides, and examples: `docs/`
- Exact public SDK declarations for this release: `sdk/index.d.ts`
- SDK package overview and examples: `sdk/README.md`
- Bundled SDK release version: `sdk/package.json`

All paths above are relative to this extracted bundle directory. They are not paths in the Pedelec repository or in the target application.

## Source-of-truth priority

This bundle is a versioned snapshot from the GitHub Release that provided it. Its SDK declaration and documentation describe that release, but they must not override a different SDK version already installed in the target project.

When integrating into a target project, use sources in this order:

1. The target project's explicit requirements or integration specification.
2. Existing integration code in the target project.
3. The target project's `package.json` and lockfile.
4. The target project's `node_modules/@kaoruisaac/pedelec/dist/index.d.ts`.
5. The target project's installed Pedelec package README.
6. `sdk/index.d.ts` from this bundle.
7. `sdk/README.md` from this bundle.
8. `docs/` from this bundle.
9. Online official Pedelec documentation.

The installed declaration in the target project is the final API signature authority for that project. Use this bundle's declaration to understand the matching GitHub Release when the target project does not already provide a more specific SDK source.

## How to use this bundle

1. Extract this archive.
2. Put the extracted folder in, or reference it from, your project workspace.
3. Tell your Coding Agent: “Read `START_HERE.md` and integrate Pedelec into this project.”

The `sdk/` directory is reference material for the Coding Agent, not an application runtime package to copy into the target project. Install the actual `@kaoruisaac/pedelec` dependency through the target project's normal package manager.

This bundle is a Coding Agent integration resource. It is not the Pedelec Desktop App or Chrome Extension installer for end users.

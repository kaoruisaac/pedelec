---
name: pedelec-integration
description: Integrate new or existing browser applications with @kaoruisaac/pedelec, including browser-only SDK setup, session lifecycle, product tools, connection and approval UX, Provider and effort-level setup, task feedback, and verification.
---

# Pedelec Integration

Use this skill when a developer asks to connect a web application to Pedelec, add Pedelec tools, build connection or setup UI, handle session lifecycle / reconnect / approval, configure Provider and effort selection, or review whether an integration is complete. It applies to React, SolidJS, Vue, Astro client islands, vanilla frontends, and other browser applications, including both new and existing projects.

This is an integration workflow and design guardrail, not a complete SDK API reference. The target project's architecture and installed SDK declarations take precedence.

## Workflow

1. **Inspect the project before designing.** Identify the frontend framework, SSR or hydration boundaries, browser entry lifecycle, state management, design system, settings and toolbar surfaces, primary user workflow, tests, existing Pedelec code, and the installed `@kaoruisaac/pedelec` version. Do not create a usable client at module scope before confirming the browser lifecycle.
2. **Classify the dependency.** Decide whether the whole product or only a feature is Optional, Partial-required, or Required. Ask whether the user's primary purpose remains possible without Pedelec; read [integration levels](./references/integration-levels.md).
3. **Inventory product capabilities.** Separate Agent-assisted features, browser-local interactions that should not be tools, read tools, mutations, confirmation requirements, ownership/version checks, and manual alternatives.
4. **Design every tool contract before implementation.** Record its product-intent name, purpose, complete required and optional arguments, omitted-argument behavior, constraints, state read and changed, success evidence, failure behavior, duplicate risk, and confirmation policy. Read [tool design](./references/tool-design.md).
5. **Apply the baseline Pedelec UI capabilities, then add product-specific ones.** Status / Connection, Setup / Recovery, and a site-local Provider Setting are baseline capabilities unless the product requirement explicitly excludes one. The Provider selector must be populated from `listProviders()` after approval and must not hard-code the supported Provider list. Task Feedback is required for non-instant Agent work, and Agent Chat is required when conversation is the primary interaction. Current Provider is an optional status-adjacent indicator; ask before implementation whether the product should show it. Effort Setting remains product-policy-dependent. Read [UI components](./references/ui-components.md).
6. **Write a short pre-implementation summary** before modifying code:

   ```text
   Pedelec Integration Summary

   Integration level:
   - Optional / Partial-required / Required

   Pedelec-dependent features:
   - ...

   Proposed tools:
   - tool name / purpose
   - required args
   - optional args and missing-value behavior
   - application state read / changed

   Required components:
   - ...

   Provider / effort policy:
   - inherit / recommend / require

   Unavailable experience:
   - Extension unavailable
   - Desktop unavailable
   - approval required
   - Provider setup incomplete
   - task failure

   Open decisions:
   - None / ...
   ```

   If the requirements are clear, make reasonable decisions and record them; do not ask questions merely to complete the template.
7. **Implement against the installed SDK.** Follow [SDK integration](./references/sdk-integration.md) and [session lifecycle](./references/session-lifecycle.md). Register listeners and required handlers before the first real turn, and keep application business rules outside generic wrappers.
8. **Implement non-happy paths.** Distinguish checking, Extension unavailable, Desktop unavailable, current-origin approval, Provider setup incomplete, ready, running, and task or connection failure. Read [connection and installation](./references/connection-and-installation.md) and [Provider / effort](./references/provider-and-effort.md).
9. **Verify before declaring completion.** Read [verification](./references/verification.md), run the project's relevant tests and builds, and report anything that could not be verified.

## SDK source of truth

Confirm technical details in this order:

1. The project's requirement or integration specification.
2. Existing Pedelec integration code.
3. `package.json` and lockfile for the installed `@kaoruisaac/pedelec` version.
4. `node_modules/@kaoruisaac/pedelec/dist/index.d.ts`.
5. The README shipped with that package.
6. Pedelec official documentation.

Installed declarations are the final source for the target project. Never infer an API from this skill, memory, another project's version, or an online example. An optional capability described here does not prove that the installed version supports it.

## Reference routing

- Read [integration levels](./references/integration-levels.md) whenever the dependency is not obvious.
- Read [SDK integration](./references/sdk-integration.md) and [session lifecycle](./references/session-lifecycle.md) for every implementation or review.
- Read [tool design](./references/tool-design.md) whenever tools are added or changed.
- Read [connection and installation](./references/connection-and-installation.md) for availability, approval, setup, or recovery UX.
- Read [Provider / effort](./references/provider-and-effort.md) whenever a session Provider or effort policy is involved.
- Read [UI components](./references/ui-components.md) when adding or reviewing product UI.
- Read [verification](./references/verification.md) before handoff.


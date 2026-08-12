# Integration levels

Classify the product and, when useful, individual features. The key question is: **if Pedelec is unavailable, can the user still complete the product's primary purpose?** Do not make an entire site Required because one enhancement uses Pedelec.

## Optional

Pedelec is an enhancement and the primary product remains usable without it.

Minimum expectations:

- Keep unrelated and manual workflows usable.
- Show status and setup guidance near the Pedelec feature, usually on demand.
- Preserve the manual alternative and return the user to the attempted action after recovery.
- Keep task feedback local to the feature; do not make Pedelec status compete with primary navigation.

Typical capabilities are a local Status / Connection indicator, setup guidance when the feature is activated, optional Provider / Effort controls or a Desktop Settings link, and local task feedback. Do not block the page at entry.

## Partial-required

Only a specific page, workspace, or workflow requires Pedelec; the rest of the product remains useful.

Minimum expectations:

- Identify dependent and independent areas clearly.
- Check readiness on entry to the dependent area or before committing to the workflow.
- Disable or intercept only the dependent action; preserve entered, selected, and unsaved work elsewhere.
- Keep status, setup / recovery, Provider / Effort settings, and task feedback discoverable in that context.
- Resume at a sensible continuation point after approval or reconnection.

## Required

The primary product purpose depends on a ready Pedelec Extension, Desktop connection, current-origin approval, and usable Desktop-owned Provider configuration.

Minimum expectations:

- Preflight when entering the primary view, before the user invests a prompt or data.
- Keep core controls visibly checking or unavailable until the probe resolves; do not make them look ready prematurely.
- Distinguish the failing layer and provide the relevant setup, Connect, Desktop Settings, or Recheck action.
- Keep persistent status, complete setup guidance, Provider / Effort policy, and task feedback in the primary flow.
- Preserve prompts, unsaved work, explanations, and saved content when core execution is unavailable.
- After recovery, perform a real Recheck and provide a route back to the original flow.

Agent Chat is not implied by any integration level. Add it only when multi-turn conversation, transcript, and streaming output are core product interactions.


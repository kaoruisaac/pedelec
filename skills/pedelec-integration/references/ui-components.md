# UI capabilities

Use the target project's existing design system and framework lifecycle. These are capabilities, not a React / Solid / Vue component API.

## Status / Connection

Represent at least these distinct user-facing states when relevant:

- checking / status unknown;
- Extension unavailable;
- Desktop unavailable;
- current-origin approval required;
- Provider setup required;
- ready;
- task running;
- attention / task or connection failure.

Do not collapse everything to “Pedelec unavailable,” and do not communicate state through color alone. Optional integrations keep status near the feature; Partial-required integrations keep it in the dependent area; Required integrations keep it visible in the primary view.

### Current Provider indicator

Showing the Provider used by the active session next to the Pedelec status is an optional capability. Ask before implementation whether the product should display it. When shown, derive the value from the active session's `session.provider`; do not treat the Provider Setting's current selection as the active Provider. A change in Provider Setting applies to a newly created session and must not relabel an already-running session.

## Setup / Recovery

Provide the action appropriate to the failure layer: Extension setup, Desktop startup / repair, Connect Pedelec, Recheck, Desktop Settings, retry, or return to an editable state. For both Extension unavailable and Desktop unavailable, provide a user-visible link to `https://pedelec.cc/download` as the standard Pedelec setup / download entry point. A direct Chrome Web Store link may also be offered for Extension setup. Opening a download or approval page is not proof that setup finished; Recheck must run a real probe. Preserve prompts, unsaved input, and recoverable work.

## Provider Setting / Effort

A site-local Provider Setting is a baseline Pedelec UI capability unless the product requirement explicitly says users must not choose a Provider in the site. Populate its Provider list or dropdown from `sdk.listProviders()` after approval; do not hard-code Provider codes or labels. By default, show Providers reported with `available: true` as selectable options. Keep an “inherit Desktop default Provider” choice when the product supports inheriting Desktop configuration.

Effort Setting remains product-policy-dependent. When included, offer `default | low | high`. Provide a route to Pedelec Desktop Settings for configuration that belongs to Desktop. Do not recreate Desktop settings and do not expose credentials, actual model mapping, or provider-native effort arguments. Read [Provider / effort](./provider-and-effort.md).

## Task Feedback

For any non-instant task, show started, processing, waiting for user / interaction, success, and failure states as applicable. Explain user-understandable work, not private chain-of-thought or invented percentages. Preserve retry or correction paths and do not leave a permanent “Thinking…” state.

## Agent Chat

Add chat only when multi-turn conversation, transcript retention, and streaming Agent output are the primary interaction. Distinguish user, Agent, and system messages; append deltas to one Agent message; retain useful partial output after failure; clear typing indicators on completion, failure, cancellation, and session end; and make tool waiting visible as an action request. A form or command-driven product can use task feedback without Chat.


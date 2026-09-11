# Provider and Effort Level

The current Web SDK contract is **Provider + provider-independent Effort Level**, not arbitrary Web-side model selection.

## Session semantics

Session input may use:

```text
provider: an installed Provider code, or omitted to inherit Desktop defaultProvider
effortLevel: default | low | high
```

If effort is omitted, use the SDK's documented default behavior, currently `default` in the supported contract. The three levels are product-facing profiles; they do not promise a universal native provider argument or model.

`getSettings()` exposes only `defaultProvider`. `listProviders()` exposes public Provider discovery such as display name, code, availability, `isDefault`, and an optional diagnostic error. `isDefault` reflects the current Desktop default provider and is independent of availability. The Web App must not expect either API to expose actual model mappings, provider-native effort arguments, credentials, API keys, executable paths, endpoint configuration, or the complete Desktop settings object.

## Provider Setting baseline

A Pedelec-integrated product must expose a site-local Provider Setting by default unless the product requirement explicitly says users must not choose a Provider in the site. After approval, populate the Provider list or dropdown from `listProviders()`; do not hard-code Provider codes, labels, or availability. By default, Providers with `available: true` are the selectable choices. Unavailable Providers may be shown separately for diagnostics when useful, but they are not normal selectable options.

Changing the Provider Setting selects the Provider for a new session. It does not change the Provider of an existing session. If the UI also shows Current Provider, derive that indicator from `session.provider` rather than from the selection control.

## Product policies

- **Inherit:** expose an “inherit Desktop default Provider” choice and omit `provider` when that choice is used; use the default effort profile unless the product needs another level.
- **Recommend:** preselect or recommend a Provider or effort level for a capability, explain why, and allow override when possible.
- **Require:** restrict selection only when a genuine capability or compatibility requirement cannot be met another way; tell the user before core work starts.

Record the policy, required capabilities, override behavior, whether the choice is site-session-only, and the incompatible-state recovery path in the Integration Summary.

## UI boundary

Use a Provider picker or Provider list as the baseline site-local Provider Setting and source it from `listProviders()` after approval. Do not maintain a separate hard-coded Provider registry in the Web App. Provide an inherit Desktop default option when supported by the product policy and a Desktop Settings entry point for Desktop-owned configuration. A site-persisted selection must be labeled as that site's preference.

Effort Level is separate from the Provider Setting and is optional unless the product policy requires it. When included, use `default`, `low`, and `high`. Do not create an arbitrary Model input, Model combobox, or full Desktop settings editor.

Actual model selection and provider-native effort configuration are **Pedelec Desktop-owned configuration**. If a product has a provider-specific requirement, express it as compatibility guidance, a readiness check, or a Desktop Settings action. Do not claim the Web SDK can read or edit the model profile.


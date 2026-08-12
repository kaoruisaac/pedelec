# Connection and installation UX

Connection UX should tell the user where the problem is and what to do next. Keep these layers distinct:

1. checking / status unknown;
2. Extension unavailable;
3. Desktop unavailable;
4. current-origin approval required;
5. Provider setup incomplete;
6. ready;
7. task running;
8. task or connection failure.

“Extension unavailable” does not prove that the Extension is uninstalled. It can mean disabled, another browser profile, bridge disconnected, unsupported origin, or an unavailable connection. “Desktop unavailable” is not an Extension installation problem. A failed task is not automatically an installation problem.

## Probe guidance

- Use `getApprovalStatus()` for non-sensitive approval and connection signals.
- Use `getApprovalStatus().appConnected` as a Desktop connection signal, not Provider readiness.
- Use `checkAvailability()` for a complete Extension / approval / Desktop readiness probe when supported by the installed SDK.
- Do not use `getSettings()` or `listProviders()` as a no-approval initial probe; they require approval and may open its popup.
- A successful availability preflight does not guarantee session creation or Provider execution.

Use exact return shapes and error names from the installed declarations rather than guessing.

## Actions and entry points

- Standard Pedelec setup / download entry point: `https://pedelec.cc/download`
- Direct Chrome Extension entry point: `https://chromewebstore.google.com/detail/pedelec/ogccgaminlphbkeghldidiiimajfdpag`

For Extension unavailable, explain the browser/profile/origin possibilities, provide a visible link to `https://pedelec.cc/download`, optionally provide the direct Chrome Web Store link, and offer Recheck. For Desktop unavailable, offer Desktop startup or repair, provide a visible link to `https://pedelec.cc/download`, and offer Recheck. For approval, say that the current site needs connection permission and offer Connect Pedelec; explain manual popup opening if needed. For Provider setup, offer an available session Provider / Effort choice or Desktop Settings when actual configuration, authentication, model mapping, or native arguments must change.

Opening a link does not complete setup. Recheck must perform the actual readiness probe. Preserve user work and return to the original action after recovery. Optional guidance may be dismissed; Required core controls remain unavailable until ready, while saved work and an explanation remain accessible.


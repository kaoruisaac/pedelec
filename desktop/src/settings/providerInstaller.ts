import { invoke } from "@tauri-apps/api/core";
import type { ProviderCode } from "./types";

export type ProviderInstallerCode = Extract<
  ProviderCode,
  "codex" | "antigravity" | "opencode" | "cursor" | "claude"
>;
export type OnboardingInstallerCode = Exclude<ProviderInstallerCode, "opencode">;

export const ONBOARDING_INSTALLER_CODES: readonly OnboardingInstallerCode[] = [
  "codex",
  "claude",
  "antigravity",
  "cursor",
];

export function isProviderInstallerSupported(code: ProviderCode): code is ProviderInstallerCode {
  return code === "codex" || code === "claude" || code === "antigravity" || code === "opencode" || code === "cursor";
}

export function isOnboardingInstallerSupported(
  code: ProviderInstallerCode,
): code is OnboardingInstallerCode {
  return (ONBOARDING_INSTALLER_CODES as readonly string[]).includes(code);
}

export function openProviderInstaller(provider: ProviderInstallerCode): Promise<void> {
  return invoke<void>("open_provider_installer", { input: { provider } });
}

export function restartPedelec(): Promise<void> {
  return invoke<void>("restart_app");
}

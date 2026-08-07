import {
  isOnboardingInstallerSupported,
  type OnboardingInstallerCode,
} from "./providerInstaller";

export type OnboardingProviderCode = OnboardingInstallerCode | "claude";
export type MissingProviderPhase = "selection" | "installation-progress";

export interface RecommendedProvider {
  code: OnboardingProviderCode;
  name: string;
  description: string;
}

export const RECOMMENDED_PROVIDERS: readonly RecommendedProvider[] = [
  {
    code: "codex",
    name: "Codex",
    description: "Use OpenAI Codex to run Pedelec agent sessions.",
  },
  {
    code: "claude",
    name: "Claude Code",
    description: "Use Anthropic Claude Code as your agent provider.",
  },
  {
    code: "antigravity",
    name: "Antigravity",
    description: "Use Google's agy CLI to connect Antigravity.",
  },
  {
    code: "cursor",
    name: "Cursor",
    description: "Use Cursor Agent CLI to run agent sessions.",
  },
];

export function phaseAfterInstallerLaunch(succeeded: boolean): MissingProviderPhase {
  return succeeded ? "installation-progress" : "selection";
}

export interface MissingProviderOnboardingState {
  phase: MissingProviderPhase;
  launchingProvider: OnboardingInstallerCode | null;
  restarting: boolean;
  error: string;
}

export function createInitialMissingProviderOnboardingState(): MissingProviderOnboardingState {
  return {
    phase: "selection",
    launchingProvider: null,
    restarting: false,
    error: "",
  };
}

export interface MissingProviderOnboardingActions {
  openProviderInstaller: (provider: OnboardingInstallerCode) => Promise<void>;
  restartPedelec: () => Promise<void>;
  onStateChange?: (state: MissingProviderOnboardingState) => void;
}

export interface MissingProviderOnboardingController {
  getState: () => MissingProviderOnboardingState;
  installProvider: (provider: OnboardingProviderCode) => Promise<void>;
  restart: () => Promise<void>;
}

function formatOnboardingError(value: unknown): string {
  if (value instanceof Error && value.message) return value.message;
  if (typeof value === "string" && value) return value;
  return "Something went wrong. Please try again.";
}

export function createMissingProviderOnboardingController(
  actions: MissingProviderOnboardingActions,
): MissingProviderOnboardingController {
  let state = createInitialMissingProviderOnboardingState();

  function updateState(next: Partial<MissingProviderOnboardingState>): void {
    state = { ...state, ...next };
    actions.onStateChange?.(state);
  }

  async function installProvider(provider: OnboardingProviderCode): Promise<void> {
    if (state.phase !== "selection" || state.launchingProvider !== null || !isOnboardingInstallerSupported(provider)) {
      return;
    }

    updateState({ error: "", launchingProvider: provider });
    try {
      await actions.openProviderInstaller(provider);
      updateState({ phase: phaseAfterInstallerLaunch(true) });
    } catch (error) {
      updateState({ phase: phaseAfterInstallerLaunch(false), error: formatOnboardingError(error) });
    } finally {
      updateState({ launchingProvider: null });
    }
  }

  async function restart(): Promise<void> {
    if (state.restarting) return;

    updateState({ error: "", restarting: true });
    try {
      await actions.restartPedelec();
    } catch (error) {
      updateState({ error: formatOnboardingError(error), restarting: false });
    }
  }

  return {
    getState: () => state,
    installProvider,
    restart,
  };
}

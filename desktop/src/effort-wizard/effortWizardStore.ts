import { createStore } from "solid-js/store";
import {
  applyEffortWizardSettings,
  errorDetails,
  formatApiError,
  getEffortWizardBootstrap,
  getEffortWizardState,
  listenToEffortWizardState,
  refreshProviders as refreshProviderCommand,
  resetEffortWizard,
  startEffortWizard,
} from "./effortWizardApi";
import {
  availableWizardProviders,
  defaultWizardSelection,
  hasAvailableWizardProvider,
  isRunResumable,
  recommendationHasTier,
} from "./effortWizardSelectors";
import type { EffortLevel } from "../settings/types";
import type {
  EffortWizardBootstrap,
  EffortWizardEntryOrigin,
  EffortWizardProviderDecision,
  EffortWizardRunState,
  EffortWizardStoreState,
  EffortWizardTierDecisions,
  WizardProviderCode,
} from "./types";

const INITIAL_STATE: EffortWizardStoreState = {
  bootstrap: null,
  runState: null,
  loading: false,
  actionLoading: false,
  error: null,
  initialized: false,
  entryOrigin: "settings",
  selectedProviders: [],
  pageStage: "selection",
  settingsRefreshRevision: 0,
  decisionDrafts: {},
};

export interface EffortWizardStore {
  store: EffortWizardStoreState;
  initialize: () => Promise<void>;
  dispose: () => void;
  refreshBootstrap: () => Promise<boolean>;
  refreshProviderStatus: () => Promise<boolean>;
  openWizard: (origin: EffortWizardEntryOrigin) => Promise<void>;
  toggleProvider: (provider: WizardProviderCode) => void;
  startSelectedProviders: () => Promise<boolean>;
  setTierDecision: (runId: string, provider: WizardProviderCode, level: EffortLevel, update: boolean) => void;
  tierDecision: (runId: string, provider: WizardProviderCode, level: EffortLevel) => "update" | "keep_current";
  applyReview: () => Promise<boolean>;
  resetForSelection: () => Promise<void>;
  cancelAndNavigate: () => Promise<void>;
  retryFatal: () => Promise<void>;
  finish: () => Promise<void>;
  hasResumableRun: () => boolean;
  clearError: () => void;
}

export function createEffortWizardStore(): EffortWizardStore {
  const [store, setStore] = createStore<EffortWizardStoreState>({ ...INITIAL_STATE });
  let initialization: Promise<void> | null = null;
  let unlisten: (() => void) | undefined;
  let disposed = false;

  function applyRunState(next: EffortWizardRunState | null): void {
    setStore("runState", next);
    if (next?.bootstrap) setStore("bootstrap", next.bootstrap);
    if (next && next.status === "review_ready") initializeDecisionDraft(next);
  }

  function initializeDecisionDraft(run: EffortWizardRunState): void {
    if (store.decisionDrafts[run.runId]) return;
    const providers: Partial<Record<WizardProviderCode, EffortWizardTierDecisions>> = {};
    for (const provider of run.providers) {
      if (provider.status !== "review_ready" || !provider.recommendation) continue;
      providers[provider.provider] = {
        low: recommendationHasTier(provider.recommendation, "low") ? "update" : "keep_current",
        default: recommendationHasTier(provider.recommendation, "default") ? "update" : "keep_current",
        high: recommendationHasTier(provider.recommendation, "high") ? "update" : "keep_current",
      };
    }
    setStore("decisionDrafts", {
      ...store.decisionDrafts,
      [run.runId]: providers,
    });
  }

  async function initialize(): Promise<void> {
    if (store.initialized) return;
    if (initialization) return initialization;
    initialization = (async () => {
      setStore({ loading: true, error: null });
      try {
        const bootstrap = await getEffortWizardBootstrap();
        if (disposed) return;
        setStore("bootstrap", bootstrap);
        applyRunState(await getEffortWizardState());
        unlisten = await listenToEffortWizardState((state) => {
          if (disposed) return;
          applyRunState(state);
        });
        setStore({ initialized: true, loading: false });
      } catch (error) {
        if (!disposed) setStore({ error: formatApiError(error), loading: false });
      }
    })();
    await initialization;
  }

  function dispose(): void {
    disposed = true;
    unlisten?.();
    unlisten = undefined;
  }

  async function refreshBootstrap(): Promise<boolean> {
    try {
      const bootstrap = await getEffortWizardBootstrap();
      if (!disposed) setStore({ bootstrap, error: null });
      return true;
    } catch (error) {
      if (!disposed) setStore("error", formatApiError(error));
      return false;
    }
  }

  async function refreshProviderStatus(): Promise<boolean> {
    if (store.actionLoading) return false;
    setStore({ actionLoading: true, error: null });
    try {
      await refreshProviderCommand();
      const refreshed = await refreshBootstrap();
      if (refreshed && store.pageStage === "no_supported" && hasAvailableWizardProvider(store.bootstrap)) {
        setStore("pageStage", "selection");
        setStore("selectedProviders", defaultWizardSelection(store.entryOrigin, store.bootstrap));
      }
      setStore("actionLoading", false);
      return refreshed;
    } catch (error) {
      if (!disposed) setStore({ error: formatApiError(error), actionLoading: false });
      return false;
    }
  }

  async function openWizard(origin: EffortWizardEntryOrigin): Promise<void> {
    await initialize();
    setStore({ entryOrigin: origin, error: null });
    if (isRunResumable(store.runState)) return;
    await refreshBootstrap();
    const selectedProviders = defaultWizardSelection(origin, store.bootstrap);
    setStore({
      selectedProviders,
      pageStage: hasAvailableWizardProvider(store.bootstrap) ? "selection" : "no_supported",
    });
  }

  function toggleProvider(provider: WizardProviderCode): void {
    if (!availableWizardProviders(store.bootstrap).includes(provider)) return;
    const next = store.selectedProviders.includes(provider)
      ? store.selectedProviders.filter((item) => item !== provider)
      : [...store.selectedProviders, provider];
    setStore("selectedProviders", next.filter((item) => availableWizardProviders(store.bootstrap).includes(item)));
  }

  async function startSelectedProviders(): Promise<boolean> {
    if (store.actionLoading || store.selectedProviders.length === 0) return false;
    setStore({ actionLoading: true, error: null });
    try {
      const state = await startEffortWizard([...store.selectedProviders]);
      applyRunState(state);
      setStore("actionLoading", false);
      return true;
    } catch (error) {
      const message = formatApiError(error);
      setStore({ actionLoading: false, error: message });
      await refreshBootstrap();
      setStore("selectedProviders", store.selectedProviders.filter((provider) => availableWizardProviders(store.bootstrap).includes(provider)));
      if (!hasAvailableWizardProvider(store.bootstrap)) setStore("pageStage", "no_supported");
      setStore("error", message);
      return false;
    }
  }

  function setTierDecision(
    runId: string,
    provider: WizardProviderCode,
    level: EffortLevel,
    update: boolean,
  ): void {
    const current = store.decisionDrafts[runId] ?? {};
    const providerDecisions = current[provider];
    if (!providerDecisions) return;
    setStore("decisionDrafts", runId, {
      ...current,
      [provider]: {
        ...providerDecisions,
        [level]: update ? "update" : "keep_current",
      },
    });
  }

  function tierDecision(
    runId: string,
    provider: WizardProviderCode,
    level: EffortLevel,
  ): "update" | "keep_current" {
    return store.decisionDrafts[runId]?.[provider]?.[level] ?? "keep_current";
  }

  function reviewDecisions(): EffortWizardProviderDecision[] {
    const run = store.runState;
    if (!run?.review) return [];
    return run.review.providers
      .filter((provider) => provider.status === "review_ready" && provider.recommendation?.confirmed)
      .filter((provider) => Object.values(provider.recommendation!.confirmed).some((value) => value !== undefined && value !== null))
      .map((provider) => ({
        provider: provider.provider,
        tiers: store.decisionDrafts[run.runId]?.[provider.provider] ?? {
          low: "keep_current",
          default: "keep_current",
          high: "keep_current",
        },
      }));
  }

  async function applyReview(): Promise<boolean> {
    const run = store.runState;
    const decisions = reviewDecisions();
    if (store.actionLoading || !run || run.status !== "review_ready" || decisions.length === 0) return false;
    setStore({ actionLoading: true, error: null });
    try {
      const output = await applyEffortWizardSettings({ runId: run.runId, providers: decisions });
      applyRunState(output.state);
      setStore({ bootstrap: output.bootstrap, actionLoading: false, settingsRefreshRevision: store.settingsRefreshRevision + 1 });
      return true;
    } catch (error) {
      setStore({ actionLoading: false, error: formatApiError(error) });
      return false;
    }
  }

  async function resetForSelection(): Promise<void> {
    const selected = [...store.selectedProviders];
    let resetSucceeded = true;
    try {
      await resetEffortWizard();
    } catch (error) {
      resetSucceeded = false;
      setStore("error", formatApiError(error));
    }
    applyRunState(null);
    setStore({
      selectedProviders: selected,
      pageStage: hasAvailableWizardProvider(store.bootstrap) ? "selection" : "no_supported",
      error: resetSucceeded ? null : store.error,
    });
  }

  async function cancelAndNavigate(): Promise<void> {
    await resetForSelection();
  }

  async function retryFatal(): Promise<void> {
    const selected = [...store.selectedProviders];
    await resetForSelection();
    setStore("selectedProviders", selected.filter((provider) => availableWizardProviders(store.bootstrap).includes(provider)));
  }

  async function finish(): Promise<void> {
    try {
      await resetEffortWizard();
    } catch (error) {
      setStore("error", formatApiError(error));
    }
    applyRunState(null);
    await refreshBootstrap();
    setStore({ pageStage: hasAvailableWizardProvider(store.bootstrap) ? "selection" : "no_supported", error: null });
  }

  return {
    store,
    initialize,
    dispose,
    refreshBootstrap,
    refreshProviderStatus,
    openWizard,
    toggleProvider,
    startSelectedProviders,
    setTierDecision,
    tierDecision,
    applyReview,
    resetForSelection,
    cancelAndNavigate,
    retryFatal,
    finish,
    hasResumableRun: () => isRunResumable(store.runState),
    clearError: () => setStore("error", null),
  };
}

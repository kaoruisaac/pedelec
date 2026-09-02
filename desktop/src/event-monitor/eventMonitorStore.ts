import { createStore, produce } from "solid-js/store";
import type { ProviderCode } from "../settings/types";

export const MAX_EVENTS_PER_THREAD = 300;
export const MAX_PROTOCOL_TRAFFIC = 300;
export const MAX_RUNTIME_STDERR = 300;

export interface MonitorEvent {
  type?: string;
  seq?: number;
  receivedAt: string;
  threadId?: string;
  status?: string;
  text?: string;
  providerSessionId?: string;
  activeProviderTurnId?: string;
  runtimeProcessId?: number;
  runtimeGeneration?: number;
  runtimeAttached?: boolean;
  source?: "provider" | "core";
  provider?: ProviderCode;
  [key: string]: unknown;
}

export interface ProviderProtocolTraffic {
  type: "provider_protocol_traffic";
  provider: ProviderCode;
  runtimeGeneration: number;
  processId: number;
  threadId?: string;
  ts: string;
  direction: "client_to_provider" | "provider_to_client";
  kind: "request" | "response" | "notification" | "event";
  message: unknown;
  unmatched?: boolean;
  receivedAt: string;
  [key: string]: unknown;
}

export interface ThreadViewModel {
  threadId: string;
  status: string;
  provider?: ProviderCode;
  providerSessionId?: string;
  activeProviderTurnId?: string;
  runtimeProcessId?: number;
  runtimeGeneration?: number;
  runtimeAttached?: boolean;
  createdAt: string;
  updatedAt: string;
  eventCount: number;
  lastEventType: string;
  lastSeq?: number;
  events: MonitorEvent[];
  assistantMessages: string[];
  commandEvents: MonitorEvent[];
  rawStdout: string;
  rawStderr: string;
  toolCalls: MonitorEvent[];
  toolResults: MonitorEvent[];
  errors: MonitorEvent[];
  protocolTraffic: ProviderProtocolTraffic[];
}

export interface RuntimeSummary {
  status: string;
  processId?: number;
  generation?: number;
}

interface EventMonitorState {
  selectedThreadId: string | null;
  threadsById: Record<string, ThreadViewModel>;
  threadOrder: string[];
  totalEventCount: number;
  globalProtocolTraffic: ProviderProtocolTraffic[];
  runtimeStderr: MonitorEvent[];
  runtimeStatus: string;
  runtimeProcessId?: number;
  runtimeGeneration?: number;
  runtimeByProvider: Partial<Record<ProviderCode, RuntimeSummary>>;
  globalError: string | null;
}

export interface EventMonitorStore {
  store: EventMonitorState;
  selectThread: (threadId: string) => void;
  setGlobalError: (error: unknown) => void;
  upsertThreadEvent: (event: unknown) => void;
  upsertRuntimeDiagnostic: (event: unknown) => void;
  upsertProtocolTraffic: (event: unknown) => void;
  clearEndedThreads: () => void;
}

function createEmptyStore(): EventMonitorState {
  return {
    selectedThreadId: null,
    threadsById: {},
    threadOrder: [],
    totalEventCount: 0,
    globalProtocolTraffic: [],
    runtimeStderr: [],
    runtimeStatus: "unknown",
    runtimeProcessId: undefined,
    runtimeGeneration: undefined,
    runtimeByProvider: {},
    globalError: null,
  };
}

export function createEventMonitorStore(): EventMonitorStore {
  const [store, setStore] = createStore<EventMonitorState>(createEmptyStore());
  const dismissedThreadIds = new Set<string>();

  function selectThread(threadId: string): void {
    setStore("selectedThreadId", threadId);
  }

  function setGlobalError(error: unknown): void {
    setStore("globalError", normalizeError(error));
  }

  function upsertThreadEvent(event: unknown): void {
    const receivedAt = new Date().toISOString();
    const eventWithReceivedAt: MonitorEvent = {
      ...((event as Record<string, unknown>) || {}),
      receivedAt,
    };

    if (isRuntimeDiagnostic(eventWithReceivedAt)) {
      upsertRuntimeDiagnostic(eventWithReceivedAt);
      return;
    }

    if (!eventWithReceivedAt.threadId) {
      setGlobalError({
        message: "Ignored thread_event without threadId",
        event: eventWithReceivedAt,
      });
      return;
    }

    if (dismissedThreadIds.has(eventWithReceivedAt.threadId)) {
      return;
    }

    setStore(
      produce((draft) => {
        const threadId = eventWithReceivedAt.threadId!;
        const existing = draft.threadsById[threadId];
        const thread =
          existing ||
          createMonitorThreadViewModel({
            threadId,
            receivedAt,
          });

        thread.updatedAt = receivedAt;
        thread.eventCount += 1;
        thread.lastEventType = eventWithReceivedAt.type || "unknown";
        if (eventWithReceivedAt.seq !== undefined && eventWithReceivedAt.seq !== null) {
          thread.lastSeq = eventWithReceivedAt.seq;
        }

        applyEventToThread(thread, eventWithReceivedAt);

        draft.threadsById[threadId] = thread;
        draft.totalEventCount += 1;
        draft.threadOrder = Object.values(draft.threadsById)
          .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt))
          .map((item) => item.threadId);

        if (!draft.selectedThreadId) {
          draft.selectedThreadId = threadId;
        }
      }),
    );
  }

  function upsertRuntimeDiagnostic(event: unknown): void {
    const receivedAt = new Date().toISOString();
    const eventWithReceivedAt: MonitorEvent = {
      ...((event as Record<string, unknown>) || {}),
      receivedAt: (event as MonitorEvent)?.receivedAt || receivedAt,
    };

    if (!isRuntimeDiagnostic(eventWithReceivedAt)) {
      return;
    }

    const threadId = eventWithReceivedAt.threadId;
    if (threadId && !dismissedThreadIds.has(threadId)) {
      setStore(
        produce((draft) => {
          const existing = draft.threadsById[threadId];
          const thread =
            existing ||
            createMonitorThreadViewModel({
              threadId,
              receivedAt: eventWithReceivedAt.receivedAt,
            });
          thread.updatedAt = eventWithReceivedAt.receivedAt;
          thread.eventCount += 1;
          thread.lastEventType = eventWithReceivedAt.type || "unknown";
          applyEventToThread(thread, eventWithReceivedAt);
          draft.threadsById[threadId] = thread;
          draft.totalEventCount += 1;
          draft.threadOrder = Object.values(draft.threadsById)
            .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt))
            .map((item) => item.threadId);
          if (!draft.selectedThreadId) {
            draft.selectedThreadId = threadId;
          }
        }),
      );
    }

    setStore(
      produce((draft) => {
        if (eventWithReceivedAt.type === "provider_runtime_stderr") {
          draft.runtimeStderr = [eventWithReceivedAt, ...draft.runtimeStderr].slice(
            0,
            MAX_RUNTIME_STDERR,
          );
        }
        applyDiagnosticToRuntimeSummary(draft, eventWithReceivedAt);
      }),
    );
  }

  function upsertProtocolTraffic(event: unknown): void {
    const receivedAt = new Date().toISOString();
    const raw = (event as Record<string, unknown>) || {};
    const eventWithReceivedAt = {
      ...raw,
      receivedAt,
    } as ProviderProtocolTraffic;

    if (!isProtocolTraffic(eventWithReceivedAt)) {
      return;
    }

    const threadId = eventWithReceivedAt.threadId;
    if (threadId) {
      if (dismissedThreadIds.has(threadId)) {
        return;
      }
      setStore(
        produce((draft) => {
          const existing = draft.threadsById[threadId];
          const thread =
            existing ||
            createMonitorThreadViewModel({
              threadId,
              receivedAt,
            });
          thread.provider = eventWithReceivedAt.provider;
          thread.runtimeProcessId = eventWithReceivedAt.processId;
          thread.runtimeGeneration = eventWithReceivedAt.runtimeGeneration;
          thread.protocolTraffic = [eventWithReceivedAt, ...thread.protocolTraffic].slice(
            0,
            MAX_PROTOCOL_TRAFFIC,
          );
          draft.threadsById[threadId] = thread;
          if (!existing) {
            draft.threadOrder = Object.values(draft.threadsById)
              .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt))
              .map((item) => item.threadId);
          }
          if (!draft.selectedThreadId) {
            draft.selectedThreadId = threadId;
          }
        }),
      );
      return;
    }

    setStore(
      produce((draft) => {
        draft.globalProtocolTraffic = [
          eventWithReceivedAt,
          ...draft.globalProtocolTraffic,
        ].slice(0, MAX_PROTOCOL_TRAFFIC);
      }),
    );
  }

  function clearEndedThreads(): void {
    const endedThreadIds = store.threadOrder.filter(
      (threadId) => store.threadsById[threadId]?.status === "ended",
    );

    if (endedThreadIds.length === 0) {
      return;
    }

    for (const threadId of endedThreadIds) {
      dismissedThreadIds.add(threadId);
    }

    setStore(
      produce((draft) => {
        let removedEventCount = 0;

        for (const threadId of endedThreadIds) {
          const thread = draft.threadsById[threadId];
          if (!thread || thread.status !== "ended") {
            continue;
          }

          removedEventCount += thread.eventCount;
          delete draft.threadsById[threadId];
        }

        draft.threadOrder = draft.threadOrder.filter(
          (threadId) => !endedThreadIds.includes(threadId),
        );
        draft.totalEventCount -= removedEventCount;

        if (
          draft.selectedThreadId !== null &&
          !draft.threadsById[draft.selectedThreadId]
        ) {
          draft.selectedThreadId = draft.threadOrder[0] || null;
        }
      }),
    );
  }

  return {
    store,
    selectThread,
    setGlobalError,
    upsertThreadEvent,
    upsertRuntimeDiagnostic,
    upsertProtocolTraffic,
    clearEndedThreads,
  };
}

function createMonitorThreadViewModel({
  threadId,
  receivedAt,
}: {
  threadId: string;
  receivedAt: string;
}): ThreadViewModel {
  return {
    threadId,
    status: "unknown",
    provider: undefined,
    providerSessionId: undefined,
    activeProviderTurnId: undefined,
    runtimeProcessId: undefined,
    runtimeGeneration: undefined,
    runtimeAttached: undefined,
    createdAt: receivedAt,
    updatedAt: receivedAt,
    eventCount: 0,
    lastEventType: "-",
    lastSeq: undefined,
    events: [],
    assistantMessages: [],
    commandEvents: [],
    rawStdout: "",
    rawStderr: "",
    toolCalls: [],
    toolResults: [],
    errors: [],
    protocolTraffic: [],
  };
}

function applyEventToThread(thread: ThreadViewModel, event: MonitorEvent): void {
  thread.events = [event, ...thread.events].slice(0, MAX_EVENTS_PER_THREAD);
  if (event.provider) {
    thread.provider = event.provider;
  }

  switch (event.type) {
    case "created":
      thread.createdAt = thread.createdAt || event.receivedAt;
      break;
    case "status_changed":
      thread.status = event.status || thread.status;
      break;
    case "raw_stdout":
      thread.rawStdout += event.text || "";
      break;
    case "raw_stderr":
      thread.rawStderr += event.text || "";
      break;
    case "assistant_message":
      thread.assistantMessages.push(event.text || "");
      break;
    case "tool_call":
      thread.toolCalls.push(event);
      break;
    case "tool_result":
      thread.toolResults.push(event);
      break;
    case "provider_command_started":
      thread.commandEvents.push(event);
      break;
    case "provider_session_id_updated":
      thread.providerSessionId = event.providerSessionId as string | undefined;
      break;
    case "error":
      thread.status = "error";
      thread.errors.push(event);
      break;
    case "ended":
      thread.status = "ended";
      break;
    case "provider_runtime_attached":
      thread.providerSessionId = event.providerThreadId as string | undefined;
      thread.runtimeProcessId = event.processId as number | undefined;
      thread.runtimeGeneration = event.runtimeGeneration as number | undefined;
      thread.runtimeAttached = true;
      break;
    case "provider_runtime_turn_started":
      thread.providerSessionId = event.providerThreadId as string | undefined;
      thread.activeProviderTurnId = event.providerTurnId as string | undefined;
      thread.runtimeProcessId = event.processId as number | undefined;
      thread.runtimeGeneration = event.runtimeGeneration as number | undefined;
      thread.runtimeAttached = true;
      break;
    case "provider_runtime_turn_completed":
      thread.activeProviderTurnId = undefined;
      thread.runtimeProcessId = event.processId as number | undefined;
      thread.runtimeGeneration = event.runtimeGeneration as number | undefined;
      break;
    case "provider_runtime_disconnected":
      thread.runtimeAttached = false;
      thread.activeProviderTurnId = undefined;
      thread.runtimeProcessId = event.processId as number | undefined;
      thread.runtimeGeneration = event.runtimeGeneration as number | undefined;
      break;
    case "provider_runtime_error":
      thread.errors.push(event);
      break;
    default:
      break;
  }
}

function isRuntimeDiagnostic(event: MonitorEvent): boolean {
  return typeof event.type === "string" && event.type.startsWith("provider_runtime_");
}

function isProtocolTraffic(event: ProviderProtocolTraffic): boolean {
  return (
    event.type === "provider_protocol_traffic" &&
    isPersistentRuntimeProvider(event.provider) &&
    typeof event.runtimeGeneration === "number" &&
    typeof event.processId === "number" &&
    typeof event.ts === "string" &&
    (event.direction === "client_to_provider" || event.direction === "provider_to_client") &&
    (event.kind === "request" ||
      event.kind === "response" ||
      event.kind === "notification" ||
      event.kind === "event")
  );
}

function applyDiagnosticToRuntimeSummary(
  state: EventMonitorState,
  event: MonitorEvent,
): void {
  state.runtimeProcessId = event.processId as number | undefined;
  state.runtimeGeneration = event.runtimeGeneration as number | undefined;
  const provider = event.provider;
  if (isPersistentRuntimeProvider(provider)) {
    const summary = state.runtimeByProvider[provider] || { status: "unknown" };
    summary.processId = event.processId as number | undefined;
    summary.generation = event.runtimeGeneration as number | undefined;
    state.runtimeByProvider[provider] = summary;
  }
  switch (event.type) {
    case "provider_runtime_started":
      state.runtimeStatus = "started";
      if (isPersistentRuntimeProvider(provider)) {
        state.runtimeByProvider[provider]!.status = "started";
      }
      break;
    case "provider_runtime_stopped":
      state.runtimeStatus = "stopped";
      if (isPersistentRuntimeProvider(provider)) {
        state.runtimeByProvider[provider]!.status = "stopped";
      }
      break;
    case "provider_runtime_disconnected":
      state.runtimeStatus = "disconnected";
      if (isPersistentRuntimeProvider(provider)) {
        state.runtimeByProvider[provider]!.status = "disconnected";
      }
      break;
    case "provider_runtime_attached":
      state.runtimeStatus = "attached";
      if (isPersistentRuntimeProvider(provider)) {
        state.runtimeByProvider[provider]!.status = "attached";
      }
      break;
    case "provider_runtime_turn_started":
      state.runtimeStatus = "running";
      if (isPersistentRuntimeProvider(provider)) {
        state.runtimeByProvider[provider]!.status = "running";
      }
      break;
    case "provider_runtime_turn_completed":
      state.runtimeStatus = "attached";
      if (isPersistentRuntimeProvider(provider)) {
        state.runtimeByProvider[provider]!.status = "attached";
      }
      break;
    case "provider_runtime_error":
      state.runtimeStatus = "error";
      if (isPersistentRuntimeProvider(provider)) {
        state.runtimeByProvider[provider]!.status = "error";
      }
      break;
    default:
      break;
  }
}

function isPersistentRuntimeProvider(
  provider: unknown,
): provider is "codex" | "antigravity" | "opencode" | "cursor" {
  return (
    provider === "codex" ||
    provider === "antigravity" ||
    provider === "opencode" ||
    provider === "cursor"
  );
}

function normalizeError(error: unknown): string {
  if (typeof error === "string") {
    return error;
  }

  try {
    return JSON.stringify(error, null, 2);
  } catch {
    return String(error);
  }
}

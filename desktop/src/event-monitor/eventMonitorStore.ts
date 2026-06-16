import { createStore, produce } from "solid-js/store";

export const MAX_EVENTS_PER_THREAD = 300;

export interface MonitorEvent {
  type?: string;
  seq?: number;
  receivedAt: string;
  threadId?: string;
  status?: string;
  text?: string;
  providerSessionId?: string;
  [key: string]: unknown;
}

export interface ThreadViewModel {
  threadId: string;
  status: string;
  providerSessionId?: string;
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
}

interface EventMonitorState {
  selectedThreadId: string | null;
  threadsById: Record<string, ThreadViewModel>;
  threadOrder: string[];
  totalEventCount: number;
  globalError: string | null;
}

export interface EventMonitorStore {
  store: EventMonitorState;
  selectThread: (threadId: string) => void;
  setGlobalError: (error: unknown) => void;
  upsertThreadEvent: (event: unknown) => void;
}

const EMPTY_STORE: EventMonitorState = {
  selectedThreadId: null,
  threadsById: {},
  threadOrder: [],
  totalEventCount: 0,
  globalError: null,
};

export function createEventMonitorStore(): EventMonitorStore {
  const [store, setStore] = createStore<EventMonitorState>(EMPTY_STORE);

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

    if (!eventWithReceivedAt.threadId) {
      setGlobalError({
        message: "Ignored thread_event without threadId",
        event: eventWithReceivedAt,
      });
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

  return {
    store,
    selectThread,
    setGlobalError,
    upsertThreadEvent,
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
    providerSessionId: undefined,
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
  };
}

function applyEventToThread(thread: ThreadViewModel, event: MonitorEvent): void {
  thread.events = [event, ...thread.events].slice(0, MAX_EVENTS_PER_THREAD);

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
    default:
      break;
  }
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

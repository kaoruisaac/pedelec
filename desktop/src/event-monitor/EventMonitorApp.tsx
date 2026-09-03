import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { JSX } from "solid-js";
import { listen } from "@tauri-apps/api/event";
import { FaRegularFolderOpen, FaSolidStop, FaSolidTrash } from "solid-icons/fa";
import {
  commandDetails,
  errorTitle,
  formatTimestamp,
  formatValue,
  prettyJson,
  statusLabel,
  toolCallDetails,
  toolResultDetails,
} from "./eventMonitorFormatters";
import { monitorEndThread, openThreadWorkspace, sendThreadText } from "./eventMonitorActions";
import { createEventMonitorStore } from "./eventMonitorStore";
import type {
  MonitorEvent,
  ProviderProtocolTraffic,
  RuntimeSummary,
  ThreadViewModel,
} from "./eventMonitorStore";

export function EventMonitorApp() {
  const monitor = createEventMonitorStore();
  const {
    store,
    clearEndedThreads,
    selectThread,
    setGlobalError,
    upsertProtocolTraffic,
    upsertThreadEvent,
  } = monitor;
  const [debugPrompt, setDebugPrompt] = createSignal("");
  const [isSubmittingDebugPrompt, setIsSubmittingDebugPrompt] = createSignal(false);
  const [stoppingThreadIds, setStoppingThreadIds] = createSignal<Set<string>>(new Set());
  let previousSelectedThreadId = store.selectedThreadId;

  const threadList = createMemo(() =>
    store.threadOrder.map((threadId) => store.threadsById[threadId]).filter(Boolean),
  );
  const selectedThread = createMemo(() =>
    store.selectedThreadId ? store.threadsById[store.selectedThreadId] : null,
  );
  const hasEvents = createMemo(() => store.totalEventCount > 0);
  const hasEndedThreads = createMemo(() =>
    threadList().some((thread) => thread.status === "ended"),
  );

  createEffect(() => {
    const selectedThreadId = store.selectedThreadId;
    if (selectedThreadId !== previousSelectedThreadId) {
      setDebugPrompt("");
      previousSelectedThreadId = selectedThreadId;
    }
  });

  async function handleOpenThreadWorkspace(threadId: string): Promise<void> {
    try {
      await openThreadWorkspace(threadId);
    } catch (error) {
      setGlobalError(error);
    }
  }

  async function handleStopThread(threadId: string): Promise<void> {
    const thread = store.threadsById[threadId];
    if (
      !thread ||
      !isThreadStoppable(thread.status) ||
      stoppingThreadIds().has(threadId)
    ) {
      return;
    }

    setStoppingThreadIds((current) => new Set(current).add(threadId));
    try {
      await monitorEndThread(threadId);
    } catch (error) {
      setGlobalError(error instanceof Error ? error.message : error);
    } finally {
      setStoppingThreadIds((current) => {
        const next = new Set(current);
        next.delete(threadId);
        return next;
      });
    }
  }

  async function handleSendDebugPrompt(threadId: string, message: string): Promise<void> {
    const trimmedMessage = message.trim();
    const selectedThread = store.threadsById[threadId];

    if (
      !trimmedMessage ||
      isSubmittingDebugPrompt() ||
      store.selectedThreadId !== threadId ||
      !isDebugPromptAvailable(selectedThread?.status)
    ) {
      return;
    }

    setIsSubmittingDebugPrompt(true);
    try {
      await sendThreadText(threadId, trimmedMessage);
      if (store.selectedThreadId === threadId) {
        setDebugPrompt("");
      }
    } catch (error) {
      setGlobalError(error instanceof Error ? error.message : error);
    } finally {
      setIsSubmittingDebugPrompt(false);
    }
  }

  onMount(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];

    const registerListener = (
      eventName: string,
      handler: (payload: unknown) => void,
    ): void => {
      listen(eventName, (event) => handler(event.payload))
        .then((cleanup) => {
          if (disposed) {
            cleanup();
          } else {
            unlisteners.push(cleanup);
          }
        })
        .catch(setGlobalError);
    };

    registerListener("thread_event", upsertThreadEvent);
    registerListener("provider_protocol_traffic", upsertProtocolTraffic);

    onCleanup(() => {
      disposed = true;
      for (const unlisten of unlisteners) {
        unlisten();
      }
    });
  });

  return (
    <main class="event-monitor-shell">
      <header class="event-monitor-topbar">
        <div class="event-monitor-title">
          <h1>Pedelec Event Monitor</h1>
          <Show
            when={hasEvents()}
            fallback={<p class="event-monitor-subtitle">Waiting for App Thread events...</p>}
          >
            <p class="event-monitor-subtitle">
              Observing App Thread activity from SDK / extension sessions.
            </p>
          </Show>
        </div>
        <div class="event-monitor-metrics" aria-label="Monitor status">
          <Metric label="Core IPC" value="unknown" status="unknown" />
          <RuntimeMetric label="Codex Runtime" summary={store.runtimeByProvider.codex} />
          <RuntimeMetric
            label="Antigravity Runtime"
            summary={store.runtimeByProvider.antigravity}
          />
          <RuntimeMetric label="OpenCode Runtime" summary={store.runtimeByProvider.opencode} />
          <RuntimeMetric label="Cursor Runtime" summary={store.runtimeByProvider.cursor} />
          <RuntimeMetric label="Claude Runtime" summary={store.runtimeByProvider.claude} />
          <Metric
            label="Latest Runtime PID / Gen"
            value={
              store.runtimeProcessId !== undefined || store.runtimeGeneration !== undefined
                ? `${formatValue(store.runtimeProcessId)} / ${formatValue(store.runtimeGeneration)}`
                : "-"
            }
          />
          <Metric label="Total sessions" value={threadList().length} />
          <Metric label="Total events" value={store.totalEventCount} />
        </div>
      </header>

      <Show when={store.globalError}>
        <pre class="event-monitor-global-error">{store.globalError}</pre>
      </Show>

      <section class="event-monitor-layout">
        <aside class="event-monitor-sidebar" aria-label="App Threads">
          <div class="event-monitor-sidebar-header">
            <h2>App Threads</h2>
            <div class="event-monitor-sidebar-header-controls">
              <span>{threadList().length}</span>
              <button
                type="button"
                class="event-monitor-icon-button"
                title="Clear ended sessions"
                aria-label="Clear ended sessions"
                disabled={!hasEndedThreads()}
                onClick={clearEndedThreads}
              >
                <FaSolidTrash size={14} />
              </button>
            </div>
          </div>

          <Show
            when={threadList().length}
            fallback={
              <div class="event-monitor-empty-sidebar">
                <p>No App Thread events yet.</p>
                <p>Start a session from SDK / extension.</p>
                <p>This desktop app will monitor events automatically.</p>
              </div>
            }
          >
            <nav class="event-monitor-thread-list">
              <For each={threadList()}>
                {(thread) => (
                  <button
                    type="button"
                    class="event-monitor-thread-item"
                    classList={{
                      "is-selected": thread.threadId === store.selectedThreadId,
                    }}
                    onClick={() => selectThread(thread.threadId)}
                  >
                    <div class="event-monitor-thread-title">
                      <span
                        class="event-monitor-thread-dot"
                        data-status={thread.status}
                        aria-hidden="true"
                      />
                      <strong>{thread.threadId}</strong>
                    </div>
                    <div class="event-monitor-thread-meta">
                      {statusLabel(thread.status)} · {thread.eventCount} events
                    </div>
                    <div class="event-monitor-thread-meta">last: {thread.lastEventType}</div>
                    <div class="event-monitor-thread-time">
                      {formatTimestamp(thread.updatedAt)}
                    </div>
                  </button>
                )}
              </For>
            </nav>
          </Show>
        </aside>

        <section class="event-monitor-main">
          <Show
            when={selectedThread()}
            fallback={
              <Show
                when={threadList().length}
                fallback={
                  <EmptyState
                    title="No App Thread events yet."
                    lines={[
                      "Start a session from SDK / extension.",
                      "This desktop app will monitor events automatically.",
                    ]}
                  />
                }
              >
                <EmptyState title="Select an App Thread from the sidebar." />
              </Show>
            }
          >
            {(thread) => (
              <ThreadDetail
                thread={thread()}
                debugPrompt={debugPrompt()}
                isSubmittingDebugPrompt={isSubmittingDebugPrompt()}
                onDebugPromptChange={setDebugPrompt}
                onSendDebugPrompt={handleSendDebugPrompt}
                onOpenWorkspace={handleOpenThreadWorkspace}
                isStopPending={stoppingThreadIds().has(thread().threadId)}
                onStopThread={handleStopThread}
                globalProtocolTraffic={store.globalProtocolTraffic}
                runtimeStderr={store.runtimeStderr}
              />
            )}
          </Show>
        </section>
      </section>
    </main>
  );
}

function Metric(props: { label: string; value: unknown; status?: string }) {
  return (
    <div class="event-monitor-metric">
      <span>{props.label}</span>
      <strong data-status={props.status}>{String(props.value)}</strong>
    </div>
  );
}

function ThreadDetail(props: {
  thread: ThreadViewModel;
  debugPrompt: string;
  isSubmittingDebugPrompt: boolean;
  onDebugPromptChange: (value: string) => void;
  onSendDebugPrompt: (threadId: string, message: string) => Promise<void>;
  onOpenWorkspace: (threadId: string) => Promise<void>;
  isStopPending: boolean;
  onStopThread: (threadId: string) => Promise<void>;
  globalProtocolTraffic: ProviderProtocolTraffic[];
  runtimeStderr: MonitorEvent[];
}) {
  const thread = () => props.thread;
  const protocolTraffic = () => {
    if (!thread().provider || thread().runtimeGeneration === undefined) {
      return thread().protocolTraffic;
    }
    return [
      ...thread().protocolTraffic,
      ...props.globalProtocolTraffic.filter(
        (event) =>
          event.provider === thread().provider &&
          event.runtimeGeneration === thread().runtimeGeneration,
      ),
    ].sort((left, right) => right.ts.localeCompare(left.ts));
  };
  const runtimeStderr = () => {
    if (!thread().provider || thread().runtimeGeneration === undefined) {
      return [];
    }
    return props.runtimeStderr.filter(
      (event) =>
        event.provider === thread().provider &&
        event.runtimeGeneration === thread().runtimeGeneration,
    );
  };

  return (
    <div class="event-monitor-detail">
      <section class="event-monitor-summary">
        <div class="event-monitor-summary-header">
          <h2>Thread Summary</h2>
          <div class="event-monitor-summary-actions">
            <button
              type="button"
              class="event-monitor-icon-button"
              title="Open workspace folder"
              aria-label="Open workspace folder"
              onClick={() => void props.onOpenWorkspace(thread().threadId)}
            >
              <FaRegularFolderOpen size={16} />
            </button>
            <button
              type="button"
              class="event-monitor-icon-button"
              title="Stop session"
              aria-label="Stop session"
              disabled={!isThreadStoppable(thread().status) || props.isStopPending}
              onClick={() => void props.onStopThread(thread().threadId)}
            >
              <FaSolidStop size={14} />
            </button>
          </div>
        </div>
        <div class="event-monitor-summary-grid">
          <SummaryItem label="Thread ID" value={thread().threadId} />
          <SummaryItem label="Status" value={thread().status} status={thread().status} />
          <SummaryItem label="Provider Session ID" value={thread().providerSessionId} />
          <SummaryItem label="Provider Turn ID" value={thread().activeProviderTurnId} />
          <SummaryItem
            label="Runtime PID / Gen"
            value={
              thread().runtimeProcessId !== undefined || thread().runtimeGeneration !== undefined
                ? `${formatValue(thread().runtimeProcessId)} / ${formatValue(thread().runtimeGeneration)}`
                : undefined
            }
          />
          <SummaryItem
            label="Runtime Attached"
            value={
              thread().runtimeAttached === undefined
                ? undefined
                : thread().runtimeAttached
                  ? "yes"
                  : "no"
            }
          />
          <SummaryItem label="Created At" value={formatTimestamp(thread().createdAt)} />
          <SummaryItem label="Updated At" value={formatTimestamp(thread().updatedAt)} />
          <SummaryItem label="Event Count" value={thread().eventCount} />
          <SummaryItem label="Last Event Type" value={thread().lastEventType} />
          <SummaryItem label="Last Seq" value={thread().lastSeq} />
        </div>
      </section>

      <DebugPrompt
        thread={thread()}
        prompt={props.debugPrompt}
        isSubmitting={props.isSubmittingDebugPrompt}
        onPromptChange={props.onDebugPromptChange}
        onSubmit={props.onSendDebugPrompt}
      />

      <div class="event-monitor-block-grid">
        <MonitorBlock title="Events" empty={thread().events.length === 0}>
          <For each={thread().events}>
            {(event) => <pre class="event-monitor-json">{prettyJson(event)}</pre>}
          </For>
        </MonitorBlock>

        <MonitorBlock title="Assistant" empty={thread().assistantMessages.length === 0}>
          <For each={thread().assistantMessages}>
            {(message) => <pre class="event-monitor-stream">{message}</pre>}
          </For>
        </MonitorBlock>

        <MonitorBlock title="Commands" empty={thread().commandEvents.length === 0}>
          <For each={thread().commandEvents}>
            {(event) => <pre class="event-monitor-json">{commandDetails(event as Record<string, unknown>)}</pre>}
          </For>
        </MonitorBlock>

        <MonitorBlock title="Protocol Traffic" empty={protocolTraffic().length === 0}>
          <For each={protocolTraffic()}>
            {(event) => <pre class="event-monitor-json">{prettyJson(event)}</pre>}
          </For>
        </MonitorBlock>

        <MonitorBlock title="Stdout" empty={!thread().rawStdout}>
          <pre class="event-monitor-stream">{thread().rawStdout}</pre>
        </MonitorBlock>

        <MonitorBlock
          title="Stderr"
          empty={!thread().rawStderr && runtimeStderr().length === 0}
        >
          <Show when={thread().rawStderr}>
            <pre class="event-monitor-stream">{thread().rawStderr}</pre>
          </Show>
          <For each={runtimeStderr()}>
            {(event) => <pre class="event-monitor-stream">{event.text || ""}</pre>}
          </For>
        </MonitorBlock>

        <MonitorBlock title="Tool Calls" empty={thread().toolCalls.length === 0}>
          <For each={thread().toolCalls}>
            {(event) => <pre class="event-monitor-json">{toolCallDetails(event as Record<string, unknown>)}</pre>}
          </For>
        </MonitorBlock>

        <MonitorBlock title="Tool Results" empty={thread().toolResults.length === 0}>
          <For each={thread().toolResults}>
            {(event) => <pre class="event-monitor-json">{toolResultDetails(event as Record<string, unknown>)}</pre>}
          </For>
        </MonitorBlock>

        <MonitorBlock title="Errors" empty={thread().errors.length === 0}>
          <For each={thread().errors}>
            {(event) => (
              <div class="event-monitor-error-event">
                <strong>{errorTitle(event)}</strong>
                <pre class="event-monitor-json">{prettyJson(event)}</pre>
              </div>
            )}
          </For>
        </MonitorBlock>
      </div>
    </div>
  );
}

function RuntimeMetric(props: { label: string; summary?: RuntimeSummary }) {
  return (
    <Metric
      label={props.label}
      value={props.summary?.status || "unknown"}
      status={props.summary?.status || "unknown"}
    />
  );
}

function isDebugPromptAvailable(status: string | undefined): boolean {
  return status === "idle" || status === "ended";
}

function isThreadStoppable(status: string | undefined): boolean {
  return status !== undefined && status !== "stopping" && status !== "ended";
}

export function DebugPrompt(props: {
  thread: ThreadViewModel;
  prompt: string;
  isSubmitting: boolean;
  onPromptChange: (value: string) => void;
  onSubmit: (threadId: string, message: string) => Promise<void>;
}) {
  const thread = () => props.thread;
  const trimmedPrompt = () => props.prompt.trim();
  const canSubmit = () =>
    isDebugPromptAvailable(thread().status) && trimmedPrompt().length > 0 && !props.isSubmitting;

  async function submit(): Promise<void> {
    if (!canSubmit()) {
      return;
    }

    const threadId = thread().threadId;
    const message = trimmedPrompt();
    await props.onSubmit(threadId, message);
  }

  return (
    <section class="event-monitor-debug-prompt">
      <div class="event-monitor-debug-prompt-header">
        <h2>Debug Prompt</h2>
      </div>
      <form
        class="event-monitor-debug-prompt-form"
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <textarea
          aria-label="Debug prompt"
          placeholder="Ask the provider agent about this session…"
          rows="4"
          value={props.prompt}
          disabled={!isDebugPromptAvailable(thread().status) || props.isSubmitting}
          onInput={(event) => props.onPromptChange(event.currentTarget.value)}
          onKeyDown={(event) => {
            if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
              event.preventDefault();
              void submit();
            }
          }}
        />
        <div class="event-monitor-debug-prompt-footer">
          <p class="event-monitor-debug-prompt-hint" role="status">
            {props.isSubmitting
              ? "Sending prompt…"
              : isDebugPromptAvailable(thread().status)
                ? "Available when this thread is idle or ended."
                : `Unavailable while this thread is ${statusLabel(thread().status)}.`}
          </p>
          <button type="submit" disabled={!canSubmit()}>
            {props.isSubmitting ? "Sending…" : "Send"}
          </button>
        </div>
      </form>
    </section>
  );
}

function SummaryItem(props: { label: string; value: unknown; status?: string }) {
  return (
    <div class="event-monitor-summary-item">
      <span>{props.label}</span>
      <strong data-status={props.status}>{formatValue(props.value)}</strong>
    </div>
  );
}

function MonitorBlock(props: { title: string; empty: boolean; children: JSX.Element }) {
  return (
    <section class="event-monitor-block">
      <div class="event-monitor-block-header">
        <h2>{props.title}</h2>
      </div>
      <div class="event-monitor-block-body">
        <Show when={!props.empty} fallback={<p class="event-monitor-empty-block">-</p>}>
          {props.children}
        </Show>
      </div>
    </section>
  );
}

function EmptyState(props: { title: string; lines?: string[] }) {
  return (
    <div class="event-monitor-empty-state">
      <h2>{props.title}</h2>
      <For each={props.lines || []}>{(line) => <p>{line}</p>}</For>
    </div>
  );
}

// Re-export MonitorEvent for external consumers
export type { MonitorEvent };

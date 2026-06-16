import { createMemo, For, onCleanup, onMount, Show } from "solid-js";
import type { JSX } from "solid-js";
import { listen } from "@tauri-apps/api/event";
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
import { createEventMonitorStore } from "./eventMonitorStore";
import type { MonitorEvent, ThreadViewModel } from "./eventMonitorStore";

export function EventMonitorApp() {
  const monitor = createEventMonitorStore();
  const { store, selectThread, setGlobalError, upsertThreadEvent } = monitor;

  const threadList = createMemo(() =>
    store.threadOrder.map((threadId) => store.threadsById[threadId]).filter(Boolean),
  );
  const selectedThread = createMemo(() =>
    store.selectedThreadId ? store.threadsById[store.selectedThreadId] : null,
  );
  const hasEvents = createMemo(() => store.totalEventCount > 0);

  onMount(() => {
    let unlisten: (() => void) | undefined;

    listen("thread_event", (event) => upsertThreadEvent(event.payload))
      .then((cleanup) => {
        unlisten = cleanup;
      })
      .catch(setGlobalError);

    onCleanup(() => {
      if (unlisten) {
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
            <span>{threadList().length}</span>
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
            {(thread) => <ThreadDetail thread={thread()} />}
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

function ThreadDetail(props: { thread: ThreadViewModel }) {
  const thread = () => props.thread;

  return (
    <div class="event-monitor-detail">
      <section class="event-monitor-summary">
        <h2>Thread Summary</h2>
        <div class="event-monitor-summary-grid">
          <SummaryItem label="Thread ID" value={thread().threadId} />
          <SummaryItem label="Status" value={thread().status} status={thread().status} />
          <SummaryItem label="Provider Session ID" value={thread().providerSessionId} />
          <SummaryItem label="Created At" value={formatTimestamp(thread().createdAt)} />
          <SummaryItem label="Updated At" value={formatTimestamp(thread().updatedAt)} />
          <SummaryItem label="Event Count" value={thread().eventCount} />
          <SummaryItem label="Last Event Type" value={thread().lastEventType} />
          <SummaryItem label="Last Seq" value={thread().lastSeq} />
        </div>
      </section>

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

        <MonitorBlock title="Stdout" empty={!thread().rawStdout}>
          <pre class="event-monitor-stream">{thread().rawStdout}</pre>
        </MonitorBlock>

        <MonitorBlock title="Stderr" empty={!thread().rawStderr}>
          <pre class="event-monitor-stream">{thread().rawStderr}</pre>
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

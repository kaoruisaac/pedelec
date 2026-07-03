import { PEDELEC_EXTENSION_ID } from "./extension-id.js";

const SDK_EXTERNAL_PORT_NAME = "pedelec-sdk-external";
const DEFAULT_BRIDGE_TIMEOUT_MS = 30_000;

export type PedelecOptions = {
  bridgeTimeoutMs?: number;
};

export type ProviderCode = "codex" | "gemini" | "opencode" | "cursor" | "claude" | "ollama";

type CreateSessionInputWithProvider = {
  provider: ProviderCode;
  model?: string;
  skillsUrls?: string[];
  autoEndOnDisconnect?: boolean;
};

type CreateSessionInputWithDefaults = {
  provider?: undefined;
  model?: never;
  skillsUrls?: string[];
  autoEndOnDisconnect?: boolean;
};

export type CreateSessionInput = CreateSessionInputWithProvider | CreateSessionInputWithDefaults;

export type ProviderInfo = {
  name: string;
  code: ProviderCode;
  path: string | null;
  available: boolean;
  error: string | null;
};

export type PedelecSettings = {
  defaultProvider: ProviderCode | null;
  defaultModels: Partial<Record<ProviderCode, string>>;
};

export type ApprovalStatus = {
  installed: boolean;
  approved: boolean;
  origin: string | null;
};

export type PedelecError = {
  code: string;
  message: string;
  details?: unknown;
};

export type PedelecSessionStatus =
  | "idle"
  | "running"
  | "waiting_tool_result"
  | "ended"
  | "error";

type ResponseMessage = {
  channelId: string;
  type: "response";
  requestId: string;
  ok: boolean;
  result?: unknown;
  error?: PedelecError;
};

type SessionEvent =
  | {
      type: "chat_delta";
      channelId: string;
      sessionId: string;
      seq?: number;
      text: string;
    }
  | {
      type: "status_changed";
      channelId: string;
      sessionId: string;
      seq?: number;
      status: PedelecSessionStatus;
    }
  | {
      type: "tool_call";
      channelId: string;
      sessionId: string;
      seq?: number;
      toolRequestId: string;
      tool: string;
      args: unknown;
    }
  | {
      type: "done";
      channelId: string;
      sessionId: string;
      seq?: number;
    }
  | {
      type: "error";
      channelId?: string;
      sessionId?: string;
      seq?: number;
      error?: PedelecError;
    }
  | {
      type: "ended";
      channelId: string;
      sessionId: string;
      seq?: number;
    };

type PortMessage = ResponseMessage | SessionEvent;

type RuntimePort = {
  postMessage: (message: unknown) => void;
  disconnect?: () => void;
  onMessage: {
    addListener: (listener: (message: unknown) => void) => void;
  };
  onDisconnect: {
    addListener: (listener: () => void) => void;
  };
};

type ChromeRuntime = {
  runtime?: {
    lastError?: { message?: string } | null;
    connect?: (extensionId: string, connectInfo: { name: string }) => RuntimePort;
  };
};

type PendingRequest = {
  resolve: (value: unknown) => void;
  reject: (error: PedelecError) => void;
  timeoutId: ReturnType<typeof setTimeout>;
};

type PendingSend = {
  resolve: () => void;
  reject: (error: PedelecError) => void;
};

type ChatHandler = (text: string) => void;
type ToolHandler = (tool: string, args: unknown) => unknown | Promise<unknown>;
type ErrorHandler = (error: PedelecError) => void;
type StatusHandler = (status: PedelecSessionStatus) => void;
type EndedHandler = () => void;

export class Pedelec {
  private readonly pageWindow: Window | null;
  private readonly channelId: string;
  private readonly bridgeTimeoutMs: number;
  private readonly pendingRequests = new Map<string, PendingRequest>();
  private readonly sessions = new Map<string, PedelecSession>();
  private readonly lastSeqBySession = new Map<string, number>();
  private nextRequestNumber = 1;
  private disconnectedError: PedelecError | null = null;
  private port: RuntimePort | null = null;

  constructor(options: PedelecOptions = {}) {
    this.pageWindow = typeof window === "undefined" ? null : window;
    this.channelId = createChannelId();
    this.bridgeTimeoutMs = Math.max(1, options.bridgeTimeoutMs ?? DEFAULT_BRIDGE_TIMEOUT_MS);

    if (!this.pageWindow) {
      this.disconnectedError = makeError(
        "EXTENSION_UNAVAILABLE",
        "Pedelec SDK must run in a browser page context."
      );
      return;
    }

    this.connectExtension();
  }

  async createSession(input: CreateSessionInput = {}): Promise<PedelecSession> {
    const resolvedOrPromise = this.resolveCreateSessionInput(input);
    const resolvedInput =
      resolvedOrPromise instanceof Promise ? await resolvedOrPromise : resolvedOrPromise;

    const result = await this.request<{ sessionId: string }>("create_session", {
      input: {
        provider: resolvedInput.provider,
        model: resolvedInput.model,
        skillsUrls: resolvedInput.skillsUrls,
        autoEndOnDisconnect: resolvedInput.autoEndOnDisconnect,
      },
    });

    if (!result.sessionId) {
      throw makeError("SDK_PROTOCOL_ERROR", "create_session response did not include sessionId");
    }

    return this.registerSession(result.sessionId, resolvedInput.provider, resolvedInput.model);
  }

  async listProviders(): Promise<ProviderInfo[]> {
    const result = await this.request<ProviderInfo[]>("list_providers");
    if (!Array.isArray(result)) {
      throw makeError("SDK_PROTOCOL_ERROR", "list_providers response was not an array");
    }
    return result;
  }

  async getSettings(): Promise<PedelecSettings> {
    const result = await this.request<PedelecSettings>("get_settings");
    if (!isSettings(result)) {
      throw makeError("SDK_PROTOCOL_ERROR", "get_settings response had invalid shape");
    }
    return result;
  }

  async getApprovalStatus(): Promise<ApprovalStatus> {
    if (this.disconnectedError) {
      return {
        installed: false,
        approved: false,
        origin: getCurrentOrigin(this.pageWindow),
      };
    }

    try {
      const result = await this.request<ApprovalStatus>("get_approval_status");
      if (!isApprovalStatus(result)) {
        throw makeError("SDK_PROTOCOL_ERROR", "get_approval_status response had invalid shape");
      }
      return result;
    } catch (err) {
      const error = normalizeError(err, "EXTENSION_UNAVAILABLE", "Pedelec extension is unavailable.");
      if (error.code === "EXTENSION_UNAVAILABLE" || error.code === "EXTENSION_DISCONNECTED") {
        return {
          installed: false,
          approved: false,
          origin: getCurrentOrigin(this.pageWindow),
        };
      }
      throw error;
    }
  }

  async resumeSession(sessionId: string): Promise<PedelecSession> {
    if (!sessionId?.trim()) {
      throw makeError("INVALID_INPUT", "sessionId is required");
    }

    const result = await this.request<{ sessionId: string }>("resume_session", {
      sessionId,
    });

    if (!result.sessionId) {
      throw makeError("SDK_PROTOCOL_ERROR", "resume_session response did not include sessionId");
    }

    return this.registerSession(result.sessionId, "", undefined);
  }

  request<T>(type: string, payload: Record<string, unknown> = {}): Promise<T> {
    if (this.disconnectedError) {
      return Promise.reject(this.disconnectedError);
    }

    if (!this.port) {
      return Promise.reject(makeError("EXTENSION_UNAVAILABLE", "Pedelec extension is unavailable."));
    }

    const requestId = `sdk_${Date.now()}_${this.nextRequestNumber++}`;
    const message = { channelId: this.channelId, type, requestId, ...payload };

    return new Promise<T>((resolve, reject) => {
      const timeoutId = setTimeout(() => {
        this.pendingRequests.delete(requestId);
        reject(
          makeError("SDK_BRIDGE_TIMEOUT", "Pedelec extension did not respond.", {
            requestId,
            type,
          })
        );
      }, this.bridgeTimeoutMs);

      this.pendingRequests.set(requestId, {
        resolve: (value) => resolve(value as T),
        reject,
        timeoutId,
      });

      try {
        this.port?.postMessage(message);
      } catch (err) {
        this.pendingRequests.delete(requestId);
        clearTimeout(timeoutId);
        reject(normalizeError(err, "EXTENSION_DISCONNECTED", "Pedelec extension disconnected."));
      }
    });
  }

  unregisterSession(sessionId: string): void {
    this.sessions.delete(sessionId);
    this.lastSeqBySession.delete(sessionId);
  }

  private resolveCreateSessionInput(input: CreateSessionInput):
    | {
        provider: ProviderCode;
        model?: string;
        skillsUrls: string[];
        autoEndOnDisconnect: boolean;
      }
    | Promise<{
    provider: ProviderCode;
    model?: string;
    skillsUrls: string[];
    autoEndOnDisconnect: boolean;
  }> {
    const raw = (input ?? {}) as {
      provider?: unknown;
      model?: unknown;
      skillsUrls?: unknown;
      autoEndOnDisconnect?: unknown;
    };
    const provider = typeof raw.provider === "string" ? raw.provider.trim() : "";
    const hasProvider = provider.length > 0;
    const hasModel = raw.model !== undefined;
    const autoEndOnDisconnect = raw.autoEndOnDisconnect !== false;

    if (!hasProvider && hasModel) {
      throw makeError("INVALID_INPUT", "model cannot be provided without provider");
    }

    const skillsUrls = Array.isArray(raw.skillsUrls) ? raw.skillsUrls : [];
    const userModel = typeof raw.model === "string" ? raw.model : undefined;

    if (!hasProvider) {
      return this.resolveDefaultCreateSessionInput(skillsUrls, autoEndOnDisconnect);
    }

    if (!isProviderCode(provider)) {
      throw makeError("INVALID_INPUT", "provider is not supported", { provider });
    }

    let model = userModel;
    if (model === undefined) {
      return this.resolveProviderOnlyCreateSessionInput(provider, skillsUrls, autoEndOnDisconnect);
    }

    return {
      provider: provider as ProviderCode,
      model,
      skillsUrls,
      autoEndOnDisconnect,
    };
  }

  private async resolveDefaultCreateSessionInput(skillsUrls: string[], autoEndOnDisconnect: boolean): Promise<{
    provider: ProviderCode;
    model?: string;
    skillsUrls: string[];
    autoEndOnDisconnect: boolean;
  }> {
    const settings = await this.getSettings();
    if (!settings.defaultProvider) {
      throw makeError(
        "DEFAULT_PROVIDER_NOT_SET",
        "Default provider is not set. Open the Pedelec desktop app Settings page and choose a default provider."
      );
    }
    await this.assertDefaultProviderAvailable(settings.defaultProvider);
    return {
      provider: settings.defaultProvider,
      model: settings.defaultModels[settings.defaultProvider] ?? undefined,
      skillsUrls,
      autoEndOnDisconnect,
    };
  }

  private async resolveProviderOnlyCreateSessionInput(
    provider: ProviderCode,
    skillsUrls: string[],
    autoEndOnDisconnect: boolean
  ): Promise<{
    provider: ProviderCode;
    model?: string;
    skillsUrls: string[];
    autoEndOnDisconnect: boolean;
  }> {
    const settings = await this.getSettings();
    return {
      provider,
      model: settings.defaultModels[provider] ?? undefined,
      skillsUrls,
      autoEndOnDisconnect,
    };
  }

  private async assertDefaultProviderAvailable(provider: ProviderCode): Promise<void> {
    const providers = await this.listProviders();
    const info = providers.find((candidate) => candidate.code === provider);
    if (!info?.available) {
      throw makeError(
        "DEFAULT_PROVIDER_UNAVAILABLE",
        "Default provider is not currently available. Open the Pedelec desktop app Settings page and choose an available provider.",
        { provider }
      );
    }
  }

  private registerSession(sessionId: string, provider: string, model: string | undefined): PedelecSession {
    const existing = this.sessions.get(sessionId);
    if (existing) return existing;

    const session = new PedelecSession(this, sessionId, provider, model);
    this.sessions.set(sessionId, session);
    return session;
  }

  private connectExtension(): void {
    const runtime = (globalThis as { chrome?: ChromeRuntime }).chrome?.runtime;
    if (!runtime?.connect) {
      this.disconnectedError = makeError("EXTENSION_UNAVAILABLE", "Pedelec extension is unavailable.");
      return;
    }

    try {
      this.port = runtime.connect(PEDELEC_EXTENSION_ID, { name: SDK_EXTERNAL_PORT_NAME });
      this.port.onMessage.addListener((message) => this.handlePortMessage(message));
      this.port.onDisconnect.addListener(() => this.handleDisconnect());
    } catch (err) {
      this.disconnectedError = normalizeError(err, "EXTENSION_UNAVAILABLE", "Pedelec extension is unavailable.");
      this.port = null;
    }
  }

  private handlePortMessage(raw: unknown): void {
    const message = raw as PortMessage;
    if (!message || typeof message !== "object") return;
    if (message.channelId && message.channelId !== this.channelId) return;

    if (message.type === "response") {
      this.handleResponse(message);
      return;
    }

    if (isSessionEvent(message) && this.isNewEvent(message)) {
      if (message.sessionId) {
        this.sessions.get(message.sessionId)?.handleEvent(message);
      } else if (message.type === "error") {
        this.broadcastError(normalizeError(message.error, "SDK_TRANSPORT_ERROR", "Pedelec transport error"));
      }
    }
  }

  private handleResponse(message: ResponseMessage): void {
    const pending = this.pendingRequests.get(message.requestId);
    if (!pending) return;

    this.pendingRequests.delete(message.requestId);
    clearTimeout(pending.timeoutId);
    if (message.ok) {
      pending.resolve(message.result);
    } else {
      pending.reject(normalizeError(message.error, "SDK_TRANSPORT_ERROR", "SDK transport request failed"));
    }
  }

  private isNewEvent(event: SessionEvent): boolean {
    if (!event.sessionId || typeof event.seq !== "number") return true;

    const lastSeq = this.lastSeqBySession.get(event.sessionId);
    if (typeof lastSeq === "number" && event.seq <= lastSeq) {
      return false;
    }

    this.lastSeqBySession.set(event.sessionId, event.seq);
    return true;
  }

  private handleDisconnect(): void {
    const runtime = (globalThis as { chrome?: ChromeRuntime }).chrome?.runtime;
    const error = normalizeError(
      runtime?.lastError,
      "EXTENSION_DISCONNECTED",
      "Pedelec extension disconnected."
    );
    this.disconnectedError = error;
    this.port = null;

    for (const pending of this.pendingRequests.values()) {
      clearTimeout(pending.timeoutId);
      pending.reject(error);
    }
    this.pendingRequests.clear();
    this.broadcastError(error);
  }

  private broadcastError(error: PedelecError): void {
    for (const session of this.sessions.values()) {
      session.handleEvent({ type: "error", sessionId: session.sessionId, error });
    }
  }
}

export class PedelecSession {
  readonly sessionId: string;
  readonly provider: string;
  readonly model?: string;

  private status: PedelecSessionStatus = "idle";
  private pendingSend: PendingSend | null = null;
  private toolHandler: ToolHandler | null = null;
  private readonly chatHandlers = new Set<ChatHandler>();
  private readonly errorHandlers = new Set<ErrorHandler>();
  private readonly statusHandlers = new Set<StatusHandler>();
  private readonly endedHandlers = new Set<EndedHandler>();

  constructor(
    private readonly client: Pedelec,
    sessionId: string,
    provider: string,
    model?: string
  ) {
    this.sessionId = sessionId;
    this.provider = provider;
    this.model = model;
  }

  sendText(text: string): Promise<void> {
    if (this.status === "ended") {
      return Promise.reject(makeError("SESSION_ENDED", "session has ended", { sessionId: this.sessionId }));
    }

    if (this.pendingSend) {
      return Promise.reject(makeError("SESSION_BUSY", "session is already running", { sessionId: this.sessionId }));
    }

    this.setStatus("running");

    const donePromise = new Promise<void>((resolve, reject) => {
      this.pendingSend = { resolve, reject };
    });

    const requestPromise = this.client
      .request("send_text", {
        sessionId: this.sessionId,
        text,
      })
      .catch((err) => {
        const error = normalizeError(err, "SEND_TEXT_FAILED", "sendText failed");
        this.clearPendingSend();
        this.emitError(error);
        throw error;
      });

    return Promise.all([requestPromise, donePromise]).then(() => undefined);
  }

  onChat(handler: ChatHandler): () => void {
    this.chatHandlers.add(handler);
    return () => this.chatHandlers.delete(handler);
  }

  onTool(handler: ToolHandler): () => void {
    this.toolHandler = handler;
    return () => {
      if (this.toolHandler === handler) {
        this.toolHandler = null;
      }
    };
  }

  onError(handler: ErrorHandler): () => void {
    this.errorHandlers.add(handler);
    return () => this.errorHandlers.delete(handler);
  }

  onStatus(handler: StatusHandler): () => void {
    this.statusHandlers.add(handler);
    return () => this.statusHandlers.delete(handler);
  }

  onEnded(handler: EndedHandler): () => void {
    this.endedHandlers.add(handler);
    return () => this.endedHandlers.delete(handler);
  }

  getStatus(): PedelecSessionStatus {
    return this.status;
  }

  async end(): Promise<void> {
    if (this.status === "ended") return;

    try {
      await this.client.request("end_session", {
        sessionId: this.sessionId,
      });
    } catch (err) {
      const error = normalizeError(err, "END_SESSION_FAILED", "end failed");
      this.emitError(error);
      throw error;
    }

    this.markEnded();
    this.client.unregisterSession(this.sessionId);
  }

  handleEvent(event: SessionEvent): void {
    if (event.type === "chat_delta") {
      for (const handler of this.chatHandlers) {
        handler(event.text);
      }
      return;
    }

    if (event.type === "status_changed") {
      this.setStatus(event.status);
      if (event.status === "idle") {
        this.resolvePendingSend();
      } else if (event.status === "ended") {
        this.markEnded();
      } else if (event.status === "error") {
        const error = makeError("SESSION_ERROR", "session entered error status", { sessionId: this.sessionId });
        this.rejectPendingSend(error);
        this.emitError(error);
      }
      return;
    }

    if (event.type === "tool_call") {
      this.setStatus("waiting_tool_result");
      this.handleToolCall(event);
      return;
    }

    if (event.type === "done") {
      this.setStatus("idle");
      this.resolvePendingSend();
      return;
    }

    if (event.type === "error") {
      this.setStatus("error");
      const error = normalizeError(event.error, "SESSION_ERROR", "session error");
      this.rejectPendingSend(error);
      this.emitError(error);
      return;
    }

    if (event.type === "ended") {
      this.markEnded();
    }
  }

  private async handleToolCall(event: Extract<SessionEvent, { type: "tool_call" }>): Promise<void> {
    let result: unknown;
    if (!this.toolHandler) {
      result = {
        error: makeError("TOOL_HANDLER_NOT_FOUND", `No tool handler registered for ${event.tool}`, {
          tool: event.tool,
        }),
      };
    } else {
      try {
        result = await this.toolHandler(event.tool, event.args);
      } catch (err) {
        result = {
          error: normalizeError(err, "TOOL_HANDLER_ERROR", "Tool handler failed"),
        };
      }
    }

    try {
      await this.client.request("submit_tool_result", {
        sessionId: this.sessionId,
        toolRequestId: event.toolRequestId,
        result,
      });
    } catch (err) {
      this.emitError(normalizeError(err, "SUBMIT_TOOL_RESULT_FAILED", "submit_tool_result failed"));
    }
  }

  private resolvePendingSend(): void {
    const pending = this.pendingSend;
    this.pendingSend = null;
    pending?.resolve();
  }

  private rejectPendingSend(error: PedelecError): void {
    const pending = this.pendingSend;
    this.pendingSend = null;
    pending?.reject(error);
  }

  private clearPendingSend(): void {
    this.pendingSend = null;
  }

  private markEnded(): void {
    const wasEnded = this.status === "ended";
    this.setStatus("ended");
    this.rejectPendingSend(makeError("SESSION_ENDED", "session has ended", { sessionId: this.sessionId }));
    if (!wasEnded) {
      for (const handler of this.endedHandlers) {
        handler();
      }
    }
  }

  private emitError(error: PedelecError): void {
    for (const handler of this.errorHandlers) {
      handler(error);
    }
  }

  private setStatus(status: PedelecSessionStatus): void {
    if (this.status === status) return;
    this.status = status;
    for (const handler of this.statusHandlers) {
      handler(status);
    }
  }
}

function isSessionEvent(message: PortMessage): message is SessionEvent {
  return (
    message.type === "chat_delta" ||
    message.type === "status_changed" ||
    message.type === "tool_call" ||
    message.type === "done" ||
    message.type === "error" ||
    message.type === "ended"
  );
}

function isSettings(value: unknown): value is PedelecSettings {
  if (!value || typeof value !== "object") return false;
  if (Array.isArray(value)) return false;
  const settings = value as Partial<PedelecSettings> & { defaultModel?: unknown };
  const defaultModels = settings.defaultModels;
  if (settings.defaultModel !== undefined) return false;
  if (!defaultModels || typeof defaultModels !== "object" || Array.isArray(defaultModels)) {
    return false;
  }
  return (
    (settings.defaultProvider === null ||
      settings.defaultProvider === "codex" ||
      settings.defaultProvider === "gemini" ||
      settings.defaultProvider === "opencode" ||
      settings.defaultProvider === "cursor" ||
      settings.defaultProvider === "claude" ||
      settings.defaultProvider === "ollama") &&
    Object.entries(defaultModels).every(
      ([provider, model]) => isProviderCode(provider) && typeof model === "string"
    )
  );
}

function isApprovalStatus(value: unknown): value is ApprovalStatus {
  if (!value || typeof value !== "object") return false;
  const status = value as Partial<ApprovalStatus>;
  return (
    typeof status.installed === "boolean" &&
    typeof status.approved === "boolean" &&
    (status.origin === null || typeof status.origin === "string")
  );
}

function isProviderCode(value: string): value is ProviderCode {
  return (
    value === "codex" ||
    value === "gemini" ||
    value === "opencode" ||
    value === "cursor" ||
    value === "claude" ||
    value === "ollama"
  );
}

function makeError(code: string, message: string, details?: unknown): PedelecError {
  return details === undefined ? { code, message } : { code, message, details };
}

function createChannelId(): string {
  const random =
    typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID()
      : Math.random().toString(36).slice(2);
  return `pedelec_${Date.now()}_${random}`;
}

function getCurrentOrigin(pageWindow: Window | null): string | null {
  const origin = pageWindow?.location?.origin;
  return typeof origin === "string" && origin ? origin : null;
}

function normalizeError(err: unknown, fallbackCode: string, fallbackMessage: string): PedelecError {
  if (!err) return makeError(fallbackCode, fallbackMessage);
  if (typeof err === "string") return makeError(fallbackCode, err);
  if (err instanceof Error) return makeError(fallbackCode, err.message || fallbackMessage);

  const value = err as Partial<PedelecError>;
  if (typeof value.code === "string" && typeof value.message === "string") {
    return {
      code: value.code,
      message: value.message,
      details: value.details,
    };
  }

  return makeError(fallbackCode, fallbackMessage, err);
}

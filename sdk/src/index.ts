import { PEDELEC_EXTENSION_ID } from "./extension-id.js";

const SDK_EXTERNAL_PORT_NAME = "pedelec-sdk-external";
const DEFAULT_BRIDGE_TIMEOUT_MS = 30_000;

export type PedelecOptions = {
  bridgeTimeoutMs?: number;
};

export type ProviderCode = "codex" | "gemini" | "opencode" | "cursor" | "claude" | "ollama";

export type JsonPrimitive = string | number | boolean | null;

export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export type ToolArgsSchemaMeta<TDefault extends JsonValue = JsonValue> = {
  description?: string;
  default?: TDefault;
  examples?: TDefault[];
};

export type ToolArgsStringSchema = ToolArgsSchemaMeta<string> & {
  type: "string";
  enum?: string[];
  minLength?: number;
  maxLength?: number;
  pattern?: string;
};

export type ToolArgsNumberSchema = ToolArgsSchemaMeta<number> & {
  type: "number";
  enum?: number[];
  minimum?: number;
  maximum?: number;
};

export type ToolArgsIntegerSchema = ToolArgsSchemaMeta<number> & {
  type: "integer";
  enum?: number[];
  minimum?: number;
  maximum?: number;
};

export type ToolArgsBooleanSchema = ToolArgsSchemaMeta<boolean> & {
  type: "boolean";
  enum?: boolean[];
};

export type ToolArgsArraySchema = ToolArgsSchemaMeta<JsonValue[]> & {
  type: "array";
  items: ToolArgsSchemaNode;
  minItems?: number;
  maxItems?: number;
  uniqueItems?: boolean;
};

export type ToolArgsObjectSchema = ToolArgsSchemaMeta<Record<string, JsonValue>> & {
  type: "object";
  properties?: Record<string, ToolArgsSchemaNode>;
  required?: string[];
};

export type ToolArgsOneOfSchema = ToolArgsSchemaMeta & {
  oneOf: ToolArgsSchemaNode[];
};

export type ToolArgsSchemaNode =
  | ToolArgsStringSchema
  | ToolArgsNumberSchema
  | ToolArgsIntegerSchema
  | ToolArgsBooleanSchema
  | ToolArgsArraySchema
  | ToolArgsObjectSchema
  | ToolArgsOneOfSchema;

export type ToolArgsSchema = ToolArgsObjectSchema;

export type ToolSpecificHandler<TArgs = unknown, TResult = unknown> = (
  args: TArgs,
  ctx: ToolCallContext
) => TResult | Promise<TResult>;

export type ToolDefinition<
  TArgs = unknown,
  TResult = unknown,
  TName extends string = string,
> = {
  name: TName;
  description: string;
  argsSchema: ToolArgsSchema;
  timeoutMs?: number;
  handler?: ToolSpecificHandler<TArgs, TResult>;
};

export type SkillsInput<
  TTools extends readonly ToolDefinition[] = readonly ToolDefinition[],
> = {
  guidance: string;
  tools: TTools;
};

export type ToolNameOf<TTools extends readonly ToolDefinition[]> = Extract<
  TTools[number]["name"],
  string
>;

export type SerializableToolManifest = {
  name: string;
  description: string;
  argsSchema: ToolArgsSchema;
  timeoutMs?: number;
};

export type SerializableSkillsManifest = {
  guidance: string;
  tools: SerializableToolManifest[];
};

type CreateSessionInputWithProvider<
  TTools extends readonly ToolDefinition[] = readonly ToolDefinition[],
> = {
  provider: ProviderCode;
  model?: string;
  skills?: SkillsInput<TTools>;
  autoEndOnDisconnect?: boolean;
};

type CreateSessionInputWithDefaults<
  TTools extends readonly ToolDefinition[] = readonly ToolDefinition[],
> = {
  provider?: undefined;
  model?: never;
  skills?: SkillsInput<TTools>;
  autoEndOnDisconnect?: boolean;
};

export type CreateSessionInput<
  TTools extends readonly ToolDefinition[] = readonly ToolDefinition[],
> =
  | CreateSessionInputWithProvider<TTools>
  | CreateSessionInputWithDefaults<TTools>;

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

export type PedelecEventContext = {
  sessionId: string;
  provider: string;
  model?: string;
  sessionCreatedAt: number;
  eventReceivedAt?: number;
  eventEmittedAt: number;
  turnId?: string;
  turnStartedAt?: number;
  turnKind?: "user" | "prepare";
  source: "core" | "sdk";
};

export type ChatEventContext = PedelecEventContext & {
  type: "chat_delta";
  turnId: string;
  turnStartedAt: number;
  eventReceivedAt: number;
};

export type ToolCallContext = PedelecEventContext & {
  type: "tool_call";
  toolRequestId: string;
  tool: string;
  turnId: string;
  turnStartedAt: number;
  eventReceivedAt: number;
};

export type StatusEventContext = PedelecEventContext & {
  type: "status_changed" | "sdk_status_changed";
  status: PedelecSessionStatus;
  previousStatus: PedelecSessionStatus;
};

export type ErrorEventContext = PedelecEventContext & {
  type: "error" | "sdk_error";
};

export type EndedEventContext = PedelecEventContext & {
  type: "ended" | "sdk_ended";
};

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

type ActiveTurn = {
  turnId: string;
  turnStartedAt: number;
  kind: "user" | "prepare";
};

type EventDispatchMeta = {
  source: "core" | "sdk";
  eventReceivedAt?: number;
};

type ChatHandler = (text: string, ctx: ChatEventContext) => void;
type GenericToolHandler<TToolName extends string = string> = (
  tool: TToolName,
  args: unknown,
  ctx: ToolCallContext
) => unknown | Promise<unknown>;
type ErrorHandler = (error: PedelecError, ctx: ErrorEventContext) => void;
type StatusHandler = (status: PedelecSessionStatus, ctx: StatusEventContext) => void;
type EndedHandler = (ctx: EndedEventContext) => void;

const TOOL_NAME_PATTERN = /^[a-zA-Z][a-zA-Z0-9_.-]*$/;

export function defineTool<
  TArgs = unknown,
  TResult = unknown,
  const TName extends string = string,
>(
  tool: ToolDefinition<TArgs, TResult, TName>
): ToolDefinition<TArgs, TResult, TName> {
  return tool;
}

type NormalizedSkillsInput = {
  manifest?: SerializableSkillsManifest;
  handlers: Map<string, ToolSpecificHandler>;
};

function normalizeSkillsInput(value: unknown): NormalizedSkillsInput {
  const handlers = new Map<string, ToolSpecificHandler>();
  if (value === undefined) return { handlers };
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw makeError("INVALID_INPUT", "skills must be an object");
  }

  const skills = value as Partial<SkillsInput>;
  if (typeof skills.guidance !== "string") {
    throw makeError("INVALID_INPUT", "skills.guidance must be a string");
  }
  if (!Array.isArray(skills.tools)) {
    throw makeError("INVALID_INPUT", "skills.tools must be an array");
  }

  const seen = new Set<string>();
  const tools = skills.tools.map((tool, index) => {
    if (!tool || typeof tool !== "object" || Array.isArray(tool)) {
      throw makeError("INVALID_INPUT", "skills.tools entries must be objects", { index });
    }
    const rawTool = tool as Partial<ToolDefinition> & { input?: unknown };
    if (typeof rawTool.name !== "string" || !TOOL_NAME_PATTERN.test(rawTool.name)) {
      throw makeError("INVALID_INPUT", "tool name is invalid", { index, toolName: rawTool.name });
    }
    if (seen.has(rawTool.name)) {
      throw makeError("INVALID_INPUT", "duplicate tool name", { toolName: rawTool.name });
    }
    seen.add(rawTool.name);
    if (typeof rawTool.description !== "string" || rawTool.description.trim().length === 0) {
      throw makeError("INVALID_INPUT", "tool description must be a non-empty string", {
        toolName: rawTool.name,
      });
    }
    if (
      rawTool.timeoutMs !== undefined &&
      (!Number.isInteger(rawTool.timeoutMs) || rawTool.timeoutMs <= 0)
    ) {
      throw makeError("INVALID_INPUT", "tool timeoutMs must be a positive integer", {
        toolName: rawTool.name,
      });
    }
    if (rawTool.handler !== undefined && typeof rawTool.handler !== "function") {
      throw makeError("INVALID_INPUT", "tool handler must be a function", { toolName: rawTool.name });
    }
    if (rawTool.input !== undefined) {
      throw makeError("INVALID_INPUT", "tool input is no longer supported; use argsSchema", {
        toolName: rawTool.name,
      });
    }

    const argsSchema = normalizeToolArgsSchema(rawTool.argsSchema, rawTool.name);
    if (rawTool.handler) handlers.set(rawTool.name, rawTool.handler);
    return {
      name: rawTool.name,
      description: rawTool.description,
      argsSchema,
      ...(rawTool.timeoutMs === undefined ? {} : { timeoutMs: rawTool.timeoutMs }),
    };
  });

  return {
    manifest: {
      guidance: skills.guidance,
      tools,
    },
    handlers,
  };
}

function normalizeToolArgsSchema(argsSchema: unknown, toolName: string): ToolArgsSchema {
  if (!argsSchema || typeof argsSchema !== "object" || Array.isArray(argsSchema)) {
    throw makeError("INVALID_INPUT", "tool argsSchema must be an object", { toolName });
  }

  const schema = argsSchema as Record<string, unknown>;
  if (schema.type !== "object") {
    throw makeError("INVALID_INPUT", "tool argsSchema must describe an object", { toolName });
  }

  try {
    return JSON.parse(JSON.stringify(schema)) as ToolArgsSchema;
  } catch (err) {
    throw makeError("INVALID_INPUT", "tool argsSchema must be serializable", {
      toolName,
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

export class Pedelec {
  private readonly pageWindow: Window | null;
  private readonly channelId: string;
  private readonly bridgeTimeoutMs: number;
  private readonly pendingRequests = new Map<string, PendingRequest>();
  private readonly sessions = new Map<string, PedelecSession<string>>();
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

  async createSession(): Promise<PedelecSession<string>>;
  async createSession<const TTools extends readonly ToolDefinition[]>(
    input: CreateSessionInput<TTools>
  ): Promise<PedelecSession<ToolNameOf<TTools>>>;
  async createSession(input: CreateSessionInput = {}): Promise<PedelecSession<string>> {
    const resolvedOrPromise = this.resolveCreateSessionInput(input);
    const resolvedInput =
      resolvedOrPromise instanceof Promise ? await resolvedOrPromise : resolvedOrPromise;

    const result = await this.request<{ sessionId: string }>("create_session", {
      input: {
        provider: resolvedInput.provider,
        model: resolvedInput.model,
        skills: resolvedInput.skills,
        autoEndOnDisconnect: resolvedInput.autoEndOnDisconnect,
      },
    });

    if (!result.sessionId) {
      throw makeError("SDK_PROTOCOL_ERROR", "create_session response did not include sessionId");
    }

    return this.registerSession(
      result.sessionId,
      resolvedInput.provider,
      resolvedInput.model,
      resolvedInput.inlineToolHandlers
    );
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

  async resumeSession(sessionId: string): Promise<PedelecSession<string>> {
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
        skills?: SerializableSkillsManifest;
        inlineToolHandlers: Map<string, ToolSpecificHandler>;
        autoEndOnDisconnect: boolean;
      }
    | Promise<{
    provider: ProviderCode;
    model?: string;
    skills?: SerializableSkillsManifest;
    inlineToolHandlers: Map<string, ToolSpecificHandler>;
    autoEndOnDisconnect: boolean;
  }> {
    const raw = (input ?? {}) as {
      provider?: unknown;
      model?: unknown;
      skills?: unknown;
      autoEndOnDisconnect?: unknown;
    };
    const provider = typeof raw.provider === "string" ? raw.provider.trim() : "";
    const hasProvider = provider.length > 0;
    const hasModel = raw.model !== undefined;
    const autoEndOnDisconnect = raw.autoEndOnDisconnect !== false;
    const normalizedSkills = normalizeSkillsInput(raw.skills);

    if (!hasProvider && hasModel) {
      throw makeError("INVALID_INPUT", "model cannot be provided without provider");
    }

    const userModel = typeof raw.model === "string" ? raw.model : undefined;

    if (!hasProvider) {
      return this.resolveDefaultCreateSessionInput(normalizedSkills, autoEndOnDisconnect);
    }

    if (!isProviderCode(provider)) {
      throw makeError("INVALID_INPUT", "provider is not supported", { provider });
    }

    let model = userModel;
    if (model === undefined) {
      return this.resolveProviderOnlyCreateSessionInput(provider, normalizedSkills, autoEndOnDisconnect);
    }

    return {
      provider: provider as ProviderCode,
      model,
      skills: normalizedSkills.manifest,
      inlineToolHandlers: normalizedSkills.handlers,
      autoEndOnDisconnect,
    };
  }

  private async resolveDefaultCreateSessionInput(
    normalizedSkills: NormalizedSkillsInput,
    autoEndOnDisconnect: boolean
  ): Promise<{
    provider: ProviderCode;
    model?: string;
    skills?: SerializableSkillsManifest;
    inlineToolHandlers: Map<string, ToolSpecificHandler>;
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
      skills: normalizedSkills.manifest,
      inlineToolHandlers: normalizedSkills.handlers,
      autoEndOnDisconnect,
    };
  }

  private async resolveProviderOnlyCreateSessionInput(
    provider: ProviderCode,
    normalizedSkills: NormalizedSkillsInput,
    autoEndOnDisconnect: boolean
  ): Promise<{
    provider: ProviderCode;
    model?: string;
    skills?: SerializableSkillsManifest;
    inlineToolHandlers: Map<string, ToolSpecificHandler>;
    autoEndOnDisconnect: boolean;
  }> {
    const settings = await this.getSettings();
    return {
      provider,
      model: settings.defaultModels[provider] ?? undefined,
      skills: normalizedSkills.manifest,
      inlineToolHandlers: normalizedSkills.handlers,
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

  private registerSession(
    sessionId: string,
    provider: string,
    model: string | undefined,
    inlineToolHandlers: Map<string, ToolSpecificHandler> = new Map()
  ): PedelecSession<string> {
    const existing = this.sessions.get(sessionId);
    if (existing) {
      existing.replaceInlineToolHandlers(inlineToolHandlers);
      return existing;
    }

    const session = new PedelecSession<string>(this, sessionId, provider, model, inlineToolHandlers);
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
      const eventReceivedAt = Date.now();
      if (message.sessionId) {
        this.sessions.get(message.sessionId)?.handleEvent(message, {
          source: "core",
          eventReceivedAt,
        });
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
      session.handleEvent({ type: "error", sessionId: session.sessionId, error }, { source: "sdk" });
    }
  }
}

export class PedelecSession<TToolName extends string = string> {
  readonly sessionId: string;
  readonly provider: string;
  readonly model?: string;
  readonly sessionCreatedAt = Date.now();

  private status: PedelecSessionStatus = "idle";
  private pendingSend: PendingSend | null = null;
  private pendingPrepare: PendingSend | null = null;
  private preparePromise: Promise<void> | null = null;
  private prepared = false;
  private activeTurn: ActiveTurn | null = null;
  private genericToolHandler: GenericToolHandler<TToolName> | null = null;
  private inlineToolHandlers = new Map<string, ToolSpecificHandler>();
  private readonly namedToolHandlers = new Map<string, ToolSpecificHandler>();
  private readonly chatHandlers = new Set<ChatHandler>();
  private readonly errorHandlers = new Set<ErrorHandler>();
  private readonly statusHandlers = new Set<StatusHandler>();
  private readonly endedHandlers = new Set<EndedHandler>();

  constructor(
    private readonly client: Pedelec,
    sessionId: string,
    provider: string,
    model?: string,
    inlineToolHandlers: Map<string, ToolSpecificHandler> = new Map()
  ) {
    this.sessionId = sessionId;
    this.provider = provider;
    this.model = model;
    this.inlineToolHandlers = new Map(inlineToolHandlers);
  }

  prepare(): Promise<void> {
    if (this.status === "ended") {
      return Promise.reject(makeError("SESSION_ENDED", "session has ended", { sessionId: this.sessionId }));
    }

    if (this.pendingSend) {
      return Promise.reject(makeError("SESSION_BUSY", "session is already running", { sessionId: this.sessionId }));
    }

    if (this.prepared) {
      return Promise.resolve();
    }

    if (this.preparePromise) {
      return this.preparePromise;
    }

    const turn = createTurn("prepare");
    this.activeTurn = turn;
    this.setStatus("running", { source: "sdk" });

    const donePromise = new Promise<void>((resolve, reject) => {
      this.pendingPrepare = { resolve, reject };
    });

    const requestPromise = this.client
      .request("prepare_session", {
        sessionId: this.sessionId,
      })
      .catch((err) => {
        const error = normalizeError(err, "PREPARE_FAILED", "prepare failed");
        this.emitError(error, { source: "sdk" });
        this.clearPendingPrepare();
        this.finishActiveTurn(turn);
        throw error;
      });

    this.preparePromise = Promise.all([requestPromise, donePromise])
      .then(() => {
        this.prepared = true;
      })
      .finally(() => {
        this.preparePromise = null;
      });

    return this.preparePromise;
  }

  async sendText(text: string): Promise<void> {
    if (this.preparePromise) {
      try {
        await this.preparePromise;
      } catch {
        // prepare is an optimization; sendText falls back to the original first-run path.
      }
    }

    if (this.status === "ended") {
      return Promise.reject(makeError("SESSION_ENDED", "session has ended", { sessionId: this.sessionId }));
    }

    if (this.pendingSend || this.pendingPrepare) {
      return Promise.reject(makeError("SESSION_BUSY", "session is already running", { sessionId: this.sessionId }));
    }

    const turn = createTurn("user");
    this.activeTurn = turn;
    this.setStatus("running", { source: "sdk" });

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
        this.emitError(error, { source: "sdk" });
        this.clearPendingSend();
        this.finishActiveTurn(turn);
        throw error;
      });

    return Promise.all([requestPromise, donePromise]).then(() => undefined);
  }

  onChat(handler: ChatHandler): () => void {
    this.chatHandlers.add(handler);
    return () => this.chatHandlers.delete(handler);
  }

  onTool(handler: GenericToolHandler<TToolName>): () => void;
  onTool<TArgs = unknown, TResult = unknown>(
    toolName: TToolName,
    handler: ToolSpecificHandler<TArgs, TResult>
  ): () => void;
  onTool<TArgs = unknown, TResult = unknown>(
    toolNameOrHandler: TToolName | GenericToolHandler<TToolName>,
    maybeHandler?: ToolSpecificHandler<TArgs, TResult>
  ): () => void {
    if (typeof toolNameOrHandler === "string") {
      const toolName = toolNameOrHandler;
      if (!TOOL_NAME_PATTERN.test(toolName)) {
        throw makeError("INVALID_INPUT", "tool name is invalid", { toolName });
      }
      if (typeof maybeHandler !== "function") {
        throw makeError("INVALID_INPUT", "tool handler must be a function", { toolName });
      }
      this.namedToolHandlers.set(toolName, maybeHandler as ToolSpecificHandler);
      return () => {
        if (this.namedToolHandlers.get(toolName) === maybeHandler) {
          this.namedToolHandlers.delete(toolName);
        }
      };
    }

    const handler = toolNameOrHandler;
    this.genericToolHandler = handler;
    return () => {
      if (this.genericToolHandler === handler) {
        this.genericToolHandler = null;
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
      this.emitError(error, { source: "sdk" });
      throw error;
    }

    this.markEnded({ source: "sdk" });
    this.client.unregisterSession(this.sessionId);
  }

  handleEvent(event: SessionEvent, meta: EventDispatchMeta = { source: "sdk" }): void {
    if (event.type === "chat_delta") {
      const turn = this.requireActiveTurn("chat_delta", meta);
      if (!turn) return;
      if (turn.kind === "prepare") return;
      for (const handler of this.chatHandlers) {
        handler(event.text, this.createChatContext(meta, turn));
      }
      return;
    }

    if (event.type === "status_changed") {
      this.setStatus(event.status, meta);
      if (event.status === "idle") {
        this.resolveActivePending();
      } else if (event.status === "ended") {
        this.markEnded(meta);
      } else if (event.status === "error") {
        const error = makeError("SESSION_ERROR", "session entered error status", { sessionId: this.sessionId });
        this.emitError(error, meta);
        this.rejectActivePending(error);
      }
      return;
    }

    if (event.type === "tool_call") {
      const turn = this.requireActiveTurn("tool_call", meta);
      if (!turn) return;
      this.setStatus("waiting_tool_result", meta);
      this.handleToolCall(event, meta, turn);
      return;
    }

    if (event.type === "done") {
      this.setStatus("idle", meta);
      this.resolveActivePending();
      return;
    }

    if (event.type === "error") {
      this.setStatus("error", meta);
      const error = normalizeError(event.error, "SESSION_ERROR", "session error");
      this.emitError(error, meta);
      this.rejectActivePending(error);
      return;
    }

    if (event.type === "ended") {
      this.markEnded(meta);
    }
  }

  replaceInlineToolHandlers(handlers: Map<string, ToolSpecificHandler>): void {
    this.inlineToolHandlers = new Map(handlers);
  }

  private async handleToolCall(
    event: Extract<SessionEvent, { type: "tool_call" }>,
    meta: EventDispatchMeta,
    turn: ActiveTurn
  ): Promise<void> {
    let result: unknown;
    const namedHandler = this.namedToolHandlers.get(event.tool);
    const inlineHandler = this.inlineToolHandlers.get(event.tool);
    if (namedHandler) {
      try {
        result = await namedHandler(event.args, this.createToolCallContext(event, meta, turn));
      } catch (err) {
        result = {
          error: normalizeError(err, "TOOL_HANDLER_ERROR", "Tool handler failed"),
        };
      }
    } else if (inlineHandler) {
      try {
        result = await inlineHandler(event.args, this.createToolCallContext(event, meta, turn));
      } catch (err) {
        result = {
          error: normalizeError(err, "TOOL_HANDLER_ERROR", "Tool handler failed"),
        };
      }
    } else if (this.genericToolHandler) {
      try {
        result = await this.genericToolHandler(
          event.tool as TToolName,
          event.args,
          this.createToolCallContext(event, meta, turn)
        );
      } catch (err) {
        result = {
          error: normalizeError(err, "TOOL_HANDLER_ERROR", "Tool handler failed"),
        };
      }
    } else {
      result = {
        error: makeError("TOOL_HANDLER_NOT_FOUND", `No tool handler registered for ${event.tool}`, {
          tool: event.tool,
        }),
      };
    }

    try {
      await this.client.request("submit_tool_result", {
        sessionId: this.sessionId,
        toolRequestId: event.toolRequestId,
        result,
      });
    } catch (err) {
      this.emitError(normalizeError(err, "SUBMIT_TOOL_RESULT_FAILED", "submit_tool_result failed"), {
        source: "sdk",
      });
    }
  }

  private resolvePendingSend(): void {
    const pending = this.pendingSend;
    this.pendingSend = null;
    pending?.resolve();
    this.finishActiveTurn();
  }

  private resolvePendingPrepare(): void {
    const pending = this.pendingPrepare;
    this.pendingPrepare = null;
    pending?.resolve();
    this.finishActiveTurn();
  }

  private resolveActivePending(): void {
    if (this.activeTurn?.kind === "prepare") {
      this.resolvePendingPrepare();
    } else {
      this.resolvePendingSend();
    }
  }

  private rejectPendingSend(error: PedelecError): void {
    const pending = this.pendingSend;
    this.pendingSend = null;
    pending?.reject(error);
    this.finishActiveTurn();
  }

  private rejectPendingPrepare(error: PedelecError): void {
    const pending = this.pendingPrepare;
    this.pendingPrepare = null;
    pending?.reject(error);
    this.finishActiveTurn();
  }

  private rejectActivePending(error: PedelecError): void {
    if (this.activeTurn?.kind === "prepare") {
      this.rejectPendingPrepare(error);
    } else {
      this.rejectPendingSend(error);
    }
  }

  private clearPendingSend(): void {
    this.pendingSend = null;
  }

  private clearPendingPrepare(): void {
    this.pendingPrepare = null;
  }

  private markEnded(meta: EventDispatchMeta = { source: "sdk" }): void {
    const wasEnded = this.status === "ended";
    this.setStatus("ended", meta);
    if (!wasEnded) {
      for (const handler of this.endedHandlers) {
        handler(this.createEndedContext(meta));
      }
    }
    this.rejectPendingSend(makeError("SESSION_ENDED", "session has ended", { sessionId: this.sessionId }));
    this.rejectPendingPrepare(makeError("SESSION_ENDED", "session has ended", { sessionId: this.sessionId }));
    this.finishActiveTurn();
  }

  private emitError(error: PedelecError, meta: EventDispatchMeta = { source: "sdk" }): void {
    for (const handler of this.errorHandlers) {
      handler(error, this.createErrorContext(meta));
    }
  }

  private setStatus(status: PedelecSessionStatus, meta: EventDispatchMeta = { source: "sdk" }): void {
    if (this.status === status) return;
    const previousStatus = this.status;
    this.status = status;
    for (const handler of this.statusHandlers) {
      handler(status, this.createStatusContext(status, previousStatus, meta));
    }
  }

  private requireActiveTurn(type: "chat_delta" | "tool_call", meta: EventDispatchMeta): ActiveTurn | null {
    if (this.activeTurn) return this.activeTurn;

    this.emitError(
      makeError("SDK_PROTOCOL_ERROR", `${type} event was received without an active turn`, {
        sessionId: this.sessionId,
      }),
      { source: "sdk", eventReceivedAt: meta.eventReceivedAt }
    );
    return null;
  }

  private finishActiveTurn(turn: ActiveTurn | null = this.activeTurn): void {
    if (!turn || !this.activeTurn) return;
    if (this.activeTurn.turnId === turn.turnId) {
      this.activeTurn = null;
    }
  }

  private createBaseContext(meta: EventDispatchMeta, turn: ActiveTurn | null = this.activeTurn): PedelecEventContext {
    return {
      sessionId: this.sessionId,
      provider: this.provider,
      model: this.model,
      sessionCreatedAt: this.sessionCreatedAt,
      ...(meta.eventReceivedAt === undefined ? {} : { eventReceivedAt: meta.eventReceivedAt }),
      eventEmittedAt: Date.now(),
      ...(turn ? { turnId: turn.turnId, turnStartedAt: turn.turnStartedAt, turnKind: turn.kind } : {}),
      source: meta.source,
    };
  }

  private createChatContext(meta: EventDispatchMeta, turn: ActiveTurn): ChatEventContext {
    return {
      ...this.createBaseContext(meta, turn),
      type: "chat_delta",
      turnId: turn.turnId,
      turnStartedAt: turn.turnStartedAt,
      eventReceivedAt: meta.eventReceivedAt ?? Date.now(),
      source: "core",
    };
  }

  private createToolCallContext(
    event: Extract<SessionEvent, { type: "tool_call" }>,
    meta: EventDispatchMeta,
    turn: ActiveTurn
  ): ToolCallContext {
    return {
      ...this.createBaseContext(meta, turn),
      type: "tool_call",
      toolRequestId: event.toolRequestId,
      tool: event.tool,
      turnId: turn.turnId,
      turnStartedAt: turn.turnStartedAt,
      eventReceivedAt: meta.eventReceivedAt ?? Date.now(),
      source: "core",
    };
  }

  private createStatusContext(
    status: PedelecSessionStatus,
    previousStatus: PedelecSessionStatus,
    meta: EventDispatchMeta
  ): StatusEventContext {
    return {
      ...this.createBaseContext(meta),
      type: meta.source === "core" ? "status_changed" : "sdk_status_changed",
      status,
      previousStatus,
    };
  }

  private createErrorContext(meta: EventDispatchMeta): ErrorEventContext {
    return {
      ...this.createBaseContext(meta),
      type: meta.source === "core" ? "error" : "sdk_error",
    };
  }

  private createEndedContext(meta: EventDispatchMeta): EndedEventContext {
    return {
      ...this.createBaseContext(meta),
      type: meta.source === "core" ? "ended" : "sdk_ended",
    };
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

function createTurn(kind: ActiveTurn["kind"] = "user"): ActiveTurn {
  const random =
    typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID()
      : Math.random().toString(36).slice(2);
  return {
    turnId: `turn_${Date.now()}_${random}`,
    turnStartedAt: Date.now(),
    kind,
  };
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

import {
  Pedelec,
  defineTool,
  type ChatEventContext,
  type EndedEventContext,
  type ErrorEventContext,
  type PedelecEventContext,
  type StatusEventContext,
  type ToolCallContext,
  type Asset,
  type AssetPath,
  type PedelecAvailability,
  type ApprovalStatus,
  type ProviderInfo,
  type CreateSessionWorkspaceInput,
  type WorkspaceFolderPickerResult,
} from "./index";

function workspaceInputHasPublicType(): CreateSessionWorkspaceInput {
  return { path: "C:\\workspace\\project" };
}

async function typedOnToolNameFromCreateSession() {
  const pedelec = new Pedelec();
  const session = await pedelec.createSession({
    provider: "codex",
    skills: {
      guidance: "Use available tools.",
      tools: [
        defineTool({
          name: "get_selection",
          description: "Get selected text.",
          argsSchema: { type: "object", properties: {}, required: [] },
        }),
        defineTool({
          name: "replace_text",
          description: "Replace selected text.",
          argsSchema: { type: "object", properties: {}, required: [] },
        }),
      ],
    },
  });

  session.onTool((name, args, ctx) => {
    const allowed: "get_selection" | "replace_text" = name;
    const stillUnknown: unknown = args;
    const toolCtx: ToolCallContext = ctx;
    return { allowed, stillUnknown, toolCtx };
  });

  session.onTool("get_selection", async (args, ctx) => {
    const stillUnknown: unknown = args;
    const toolName: string = ctx.tool;
    const turnId: string = ctx.turnId;
    return { ok: true, stillUnknown, toolName, turnId };
  });

  session.onTool("replace_text", async () => {
    return { ok: true };
  });

  // @ts-expect-error tool name must come from skills.tools[].name
  session.onTool("not_exists", async () => {
    return { ok: false };
  });

  session.onTool((name, _args, ctx) => {
    // @ts-expect-error generic handler name should not be arbitrary string
    const invalid: "not_exists" = name;
    // @ts-expect-error user-facing context must not expose core seq
    const noSeq = ctx.seq;
    return noSeq ?? invalid;
  });

  session.onChat((_text, ctx) => {
    const chatCtx: ChatEventContext = ctx;
    const receivedAt: number = ctx.eventReceivedAt;
    void receivedAt;
    void chatCtx;
  });

  session.onStatus((_status, ctx) => {
    const statusCtx: StatusEventContext = ctx;
    const previous = ctx.previousStatus;
    void previous;
    void statusCtx;
  });

  session.onError((_error, ctx) => {
    const errorCtx: ErrorEventContext = ctx;
    return errorCtx;
  });

  session.onEnded((ctx) => {
    const endedCtx: EndedEventContext = ctx;
    return endedCtx;
  });
}

async function resumedSessionFallsBackToString() {
  const pedelec = new Pedelec();
  const session = await pedelec.resumeSession("thread_1");

  session.onTool((name) => {
    const anyString: string = name;
    return anyString;
  });

  session.onTool("runtime_tool_name", async () => {
    return { ok: true };
  });
}

async function noSkillsFallsBackToString() {
  const pedelec = new Pedelec();
  const session = await pedelec.createSession({
    provider: "codex",
    workspace: { path: "C:\\workspace\\project" },
  });
  const typedWorkspace: CreateSessionWorkspaceInput = workspaceInputHasPublicType();
  void typedWorkspace;

  session.onTool((name) => {
    const anyString: string = name;
    return anyString;
  });

  session.onTool("runtime_tool_name", async () => {
    return { ok: true };
  });
}

async function effortLevelPublicTypeContract() {
  const pedelec = new Pedelec();
  await pedelec.createSession({ effortLevel: "high" });
  const session = await pedelec.createSession({ provider: "codex", effortLevel: "low" });

  // @ts-expect-error the removed sandbox input is not part of the public contract
  pedelec.createSession({ provider: "codex", sandbox: { path: "C:\\workspace\\legacy" } });
  // @ts-expect-error the removed sandbox picker is not part of the public contract
  pedelec.sandboxFolderPicker();

  // @ts-expect-error model is no longer a createSession option
  pedelec.createSession({ provider: "codex", model: "gpt-5" });
  // @ts-expect-error session no longer exposes provider model
  session.model;
  // @ts-expect-error unsupported effort level
  pedelec.createSession({ effortLevel: "medium" });
}

async function listAssetsHasPublicTypes() {
  const pedelec = new Pedelec();
  const session = await pedelec.resumeSession("thread_1");
  const assets = await session.listAssets();
  assets satisfies Asset[];
  const path: AssetPath = assets[0]!.path;
  path satisfies `/${string}`;
}

async function assetPathsUseAssetsAsAnImplicitRoot() {
  const pedelec = new Pedelec();
  const session = await pedelec.resumeSession("thread_1");
  const file = new File(["asset"], "original.txt", { type: "text/plain" });
  const generated = await session.uploadAsset(file);
  generated satisfies AssetPath;
  const exact = await session.uploadAsset(file, "/img/image.txt");
  exact satisfies AssetPath;
  const namedAssetsDirectory: AssetPath = "/assets/image.txt";
  void namedAssetsDirectory;
  // @ts-expect-error asset paths must begin with a slash
  const missingSlash: AssetPath = "assets/image.txt";
  void missingSlash;
}

async function availabilityHasPublicType() {
  const pedelec = new Pedelec();
  const availability: PedelecAvailability = await pedelec.checkAvailability();
  const promise: Promise<PedelecAvailability> = pedelec.checkAvailability();
  void availability;
  void promise;
}

async function workspaceFolderPickerHasPublicType() {
  const pedelec = new Pedelec();
  const result: Promise<WorkspaceFolderPickerResult | null> = pedelec.workspaceFolderPicker();
  void result;
}

function directoryPickerIsRemoved() {
  const pedelec = new Pedelec();
  // @ts-expect-error directoryPicker was removed in favor of workspaceFolderPicker
  pedelec.directoryPicker();
}

function publicSecurityTypesAreRestricted() {
  const status: ApprovalStatus = {
    installed: true,
    approved: true,
    origin: "https://app.example.test",
    appConnected: true,
  };
  const provider = {} as ProviderInfo;
  // @ts-expect-error SDK provider metadata must not expose executable paths
  provider.path;
  return status;
}

void typedOnToolNameFromCreateSession;
void resumedSessionFallsBackToString;
void noSkillsFallsBackToString;
void effortLevelPublicTypeContract;
void listAssetsHasPublicTypes;
void availabilityHasPublicType;
void workspaceFolderPickerHasPublicType;
void directoryPickerIsRemoved;
void publicSecurityTypesAreRestricted;

const baseContext: PedelecEventContext = {
  sessionId: "thread_1",
  provider: "codex",
  sessionCreatedAt: Date.now(),
  eventEmittedAt: Date.now(),
  source: "sdk",
};

void baseContext;

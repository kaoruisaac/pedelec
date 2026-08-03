import {
  Pedelec,
  defineTool,
  type ChatEventContext,
  type EndedEventContext,
  type ErrorEventContext,
  type PedelecEventContext,
  type StatusEventContext,
  type ToolCallContext,
  type SandboxAsset,
  type SandboxAssetPath,
  type PedelecAvailability,
  type ApprovalStatus,
  type ProviderInfo,
} from "./index";

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
  const session = await pedelec.createSession({ provider: "codex" });

  session.onTool((name) => {
    const anyString: string = name;
    return anyString;
  });

  session.onTool("runtime_tool_name", async () => {
    return { ok: true };
  });
}

async function listAssetsHasPublicTypes() {
  const pedelec = new Pedelec();
  const session = await pedelec.resumeSession("thread_1");
  const assets = await session.listAssets();
  assets satisfies SandboxAsset[];
  const path: SandboxAssetPath = assets[0]!.path;
  path satisfies `/${string}`;
}

async function assetPathsUseAssetsAsAnImplicitRoot() {
  const pedelec = new Pedelec();
  const session = await pedelec.resumeSession("thread_1");
  const file = new File(["asset"], "original.txt", { type: "text/plain" });
  const generated = await session.uploadAsset(file);
  generated satisfies SandboxAssetPath;
  const exact = await session.uploadAsset(file, "/img/image.txt");
  exact satisfies SandboxAssetPath;
  const namedAssetsDirectory: SandboxAssetPath = "/assets/image.txt";
  void namedAssetsDirectory;
  // @ts-expect-error asset paths must begin with a slash
  const missingSlash: SandboxAssetPath = "assets/image.txt";
  void missingSlash;
}

async function availabilityHasPublicType() {
  const pedelec = new Pedelec();
  const availability: PedelecAvailability = await pedelec.checkAvailability();
  const promise: Promise<PedelecAvailability> = pedelec.checkAvailability();
  void availability;
  void promise;
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
void listAssetsHasPublicTypes;
void availabilityHasPublicType;
void publicSecurityTypesAreRestricted;

const baseContext: PedelecEventContext = {
  sessionId: "thread_1",
  provider: "codex",
  sessionCreatedAt: Date.now(),
  eventEmittedAt: Date.now(),
  source: "sdk",
};

void baseContext;

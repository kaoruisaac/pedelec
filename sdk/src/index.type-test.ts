import {
  Pedelec,
  PedelecWorkspace,
  defineTool,
  defineDenoModule,
  type DenoModuleDefinition,
  type ChatEventContext,
  type ChatDeltaEventContext,
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
  type WorkspaceRunResult,
  type Effort,
} from "./index";

const spriteTools = defineDenoModule({
  name: "sprite-tools",
  description: "Sprite authoring utilities.",
  entry: "./agent/sprite-tools.ts",
  usage: `import { previewActorSource } from "sprite-tools";`,
  preferStdinExecution: true,
});

const workspaceTools = defineDenoModule({
  name: "workspace-tools",
  entry: "./workspace/workspace-tools.ts",
});

const spriteName: "sprite-tools" = spriteTools.name;
const spriteDefinition: DenoModuleDefinition<"sprite-tools"> = spriteTools;
void spriteName;
void spriteDefinition;

const workspaceName: "workspace-tools" = workspaceTools.name;
void workspaceName;

// @ts-expect-error Deno Module entry remains required at authoring time
defineDenoModule({
  name: "missing-entry",
  description: "Missing entry.",
  usage: 'import "missing-entry";',
});

defineDenoModule({
  name: "non-string-entry",
  description: "Invalid entry.",
  // @ts-expect-error Deno Module entry is a build-time string authoring field
  entry: 42,
  usage: 'import "non-string-entry";',
});

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
      denoModules: [spriteTools],
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
    const messageType: "chat_message" = ctx.type;
    const receivedAt: number = ctx.eventReceivedAt;
    void receivedAt;
    void messageType;
    void chatCtx;
  });

  session.onChatDelta((_text, ctx) => {
    const chatDeltaCtx: ChatDeltaEventContext = ctx;
    const deltaType: "chat_delta" = ctx.type;
    const receivedAt: number = ctx.eventReceivedAt;
    void receivedAt;
    void deltaType;
    void chatDeltaCtx;
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

  await session.resume();
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
  });

  session.workspace.run("console.log('workspace')", { denoModules: [workspaceTools] });
  session.workspace.run("console.log('workspace')", { denoModules: [spriteTools] });

  await pedelec.createSession({
    provider: "codex",
    skills: { guidance: "Use modules.", tools: [],
      // @ts-expect-error Workspace-only declarations do not satisfy Session metadata requirements
      denoModules: [workspaceTools] },
  });

  const descriptionOnly = defineDenoModule({
    name: "description-only",
    description: "Missing usage.",
    entry: "./agent/description-only.ts",
  });
  await pedelec.createSession({
    provider: "codex",
    skills: { guidance: "Use modules.", tools: [],
      // @ts-expect-error Session metadata requires usage as well as description
      denoModules: [descriptionOnly] },
  });

  const usageOnly = defineDenoModule({
    name: "usage-only",
    entry: "./agent/usage-only.ts",
    usage: 'import "usage-only";',
  });
  await pedelec.createSession({
    provider: "codex",
    skills: { guidance: "Use modules.", tools: [],
      // @ts-expect-error Session metadata requires description as well as usage
      denoModules: [usageOnly] },
  });

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

  await pedelec.createSession({ model: "gpt-5" });
  await pedelec.createSession({ provider: "codex", model: "gpt-5", effort: "max" });
  const explicitEffort: Effort = "xhigh";
  await pedelec.createSession({ provider: "codex", model: "gpt-5", effort: explicitEffort });
  // @ts-expect-error explicit model mode cannot select a Desktop effort profile
  await pedelec.createSession({ provider: "codex", model: "gpt-5", effortLevel: "high" });
  // @ts-expect-error explicit effort requires an explicit model
  await pedelec.createSession({ provider: "codex", effort: "high" });
  // @ts-expect-error explicit model, effort, and Desktop profile are mutually exclusive
  await pedelec.createSession({ model: "gpt-5", effort: "max", effortLevel: "low" });
  // @ts-expect-error unsupported provider-native effort value
  await pedelec.createSession({ model: "gpt-5", effort: "default" });
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

async function workspacePublicTypeContract() {
  const pedelec = new Pedelec();
  const maybeWorkspace: PedelecWorkspace | null = await pedelec.openWorkspace();
  const workspace: PedelecWorkspace = await pedelec.openWorkspace("C:\\workspace\\project");
  const files: Promise<string[]> = workspace.listFiles();
  const folders: Promise<string[]> = workspace.listFolders("src");
  const run: Promise<WorkspaceRunResult> = workspace.run("console.log('hello')", { timeoutMs: 1000 });
  const session = await workspace.createSession({
    skills: {
      guidance: "Use tools.",
      tools: [defineTool({ name: "workspace_tool", description: "Tool", argsSchema: { type: "object" } })],
    },
  });
  session.workspace satisfies PedelecWorkspace;
  maybeWorkspace?.listFiles();
  void files;
  void folders;
  void run;

  // @ts-expect-error workspace is now selected through openWorkspace
  pedelec.createSession({ workspace: { path: "C:\\workspace\\project" } });
  // @ts-expect-error the removed picker API must not be public
  pedelec.workspaceFolderPicker();
}

function directoryPickerIsRemoved() {
  const pedelec = new Pedelec();
  // @ts-expect-error directoryPicker was removed in favor of openWorkspace
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
  const isDefault: boolean = provider.isDefault;
  void isDefault;
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
void workspacePublicTypeContract;
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

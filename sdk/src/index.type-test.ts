import {
  Pedelec,
  defineTool,
  type ChatEventContext,
  type EndedEventContext,
  type ErrorEventContext,
  type PedelecEventContext,
  type StatusEventContext,
  type ToolCallContext,
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

void typedOnToolNameFromCreateSession;
void resumedSessionFallsBackToString;
void noSkillsFallsBackToString;

const baseContext: PedelecEventContext = {
  sessionId: "thread_1",
  provider: "codex",
  sessionCreatedAt: Date.now(),
  eventEmittedAt: Date.now(),
  source: "sdk",
};

void baseContext;

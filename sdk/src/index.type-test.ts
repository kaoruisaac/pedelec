import { Pedelec, defineTool } from "./index";

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

  session.onTool((name, args) => {
    const allowed: "get_selection" | "replace_text" = name;
    const stillUnknown: unknown = args;
    return { allowed, stillUnknown };
  });

  session.onTool("get_selection", async (args) => {
    const stillUnknown: unknown = args;
    return { ok: true, stillUnknown };
  });

  session.onTool("replace_text", async () => {
    return { ok: true };
  });

  // @ts-expect-error tool name must come from skills.tools[].name
  session.onTool("not_exists", async () => {
    return { ok: false };
  });

  session.onTool((name) => {
    // @ts-expect-error generic handler name should not be arbitrary string
    const invalid: "not_exists" = name;
    return invalid;
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

export function prettyJson(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }

  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

export function formatTimestamp(value: unknown): string {
  if (!value) {
    return "-";
  }

  const date = new Date(String(value));
  if (Number.isNaN(date.getTime())) {
    return String(value);
  }

  return date.toLocaleString();
}

export function formatValue(value: unknown): string {
  if (value === undefined || value === null || value === "") {
    return "-";
  }

  return String(value);
}

export function statusLabel(status: unknown): string {
  return formatValue(status);
}

export function commandDetails(event: Record<string, unknown>): string {
  return prettyJson({
    seq: event.seq,
    processId: event.processId,
    program: event.program,
    args: event.args,
    cwd: event.cwd,
    prompt: event.prompt,
  });
}

export function toolCallDetails(event: Record<string, unknown>): string {
  return prettyJson({
    seq: event.seq,
    requestId: event.requestId,
    toolName: event.toolName,
    args: event.args,
    receivedAt: event.receivedAt,
  });
}

export function toolResultDetails(event: Record<string, unknown>): string {
  return prettyJson({
    seq: event.seq,
    requestId: event.requestId,
    toolName: event.toolName,
    result: event.result,
    receivedAt: event.receivedAt,
  });
}

export function errorTitle(event: unknown): string {
  const e = event as { error?: { message?: string }; message?: string } | null | undefined;
  return e?.error?.message || e?.message || "Error";
}

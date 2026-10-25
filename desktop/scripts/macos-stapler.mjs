import { spawnSync } from "node:child_process";
import { appendFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const MAX_ATTEMPTS = 3;
const RETRY_DELAYS_MS = [5000, 15000];

function invokeSystemXcrun(args, capture) {
  const result = spawnSync("/usr/bin/xcrun", args, {
    encoding: "utf8",
    stdio: capture ? "pipe" : "inherit",
  });
  return {
    status: result.status,
    error: result.error,
    stdout: result.stdout || "",
    stderr: result.stderr || "",
  };
}

function recordDiagnostics(message) {
  const line = `[${new Date().toISOString()}] ${message}\n`;
  if (process.env.PEDELEC_STAPLER_LOG_PATH) {
    appendFileSync(process.env.PEDELEC_STAPLER_LOG_PATH, line);
  } else {
    process.stderr.write(line);
  }
}

function describeResult(command, result, record) {
  const status = result.error ? `spawn error: ${result.error.message}` : `exit status: ${result.status ?? "unknown"}`;
  record(`${command}: ${status}`);
  if (result.stdout?.trim()) record(`${command} stdout:\n${result.stdout.trim()}`);
  if (result.stderr?.trim()) record(`${command} stderr:\n${result.stderr.trim()}`);
}

/**
 * Tauri 2.11.5 does not inspect xcrun stapler's child exit status.
 * Verify and retry here, before Tauri packages the signed .app into
 * DMG and updater assets. The workflow also has a strict post-build gate.
 */
export async function runXcrun(args, {
  invoke = invokeSystemXcrun,
  delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  record = recordDiagnostics,
  maxAttempts = MAX_ATTEMPTS,
  retryDelaysMs = RETRY_DELAYS_MS,
} = {}) {
  if (args[0] !== "stapler" || args[1] !== "staple") {
    const result = invoke(args, false);
    return result.error ? 1 : (result.status ?? 1);
  }

  const target = args.at(-1);
  if (!target || target.startsWith("-")) {
    record("Refusing stapler staple without an artifact path.");
    return 1;
  }

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    record(`Stapling ${target}: attempt ${attempt}/${maxAttempts}`);
    const staple = invoke(args, true);
    describeResult("stapler staple", staple, record);

    if (!staple.error && staple.status === 0) {
      const validate = invoke(["stapler", "validate", target], true);
      describeResult("stapler validate", validate, record);
      if (!validate.error && validate.status === 0) {
        record(`Stapling and ticket validation succeeded: ${target}`);
        return 0;
      }
    }

    if (attempt < maxAttempts) {
      const waitMs = retryDelaysMs[attempt - 1] ?? 15000;
      record(`Stapling not verified; retrying after ${waitMs}ms`);
      await delay(waitMs);
    }
  }

  record(`Stapling failed after ${maxAttempts} attempts: ${target}`);
  return 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await runXcrun(process.argv.slice(2));
}

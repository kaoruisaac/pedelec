import test from "node:test";
import assert from "node:assert/strict";
import { runXcrun } from "./macos-stapler.mjs";

const stapleArgs = ["stapler", "staple", "-v", "Pedelec.app"];
const validateArgs = ["stapler", "validate", "Pedelec.app"];
const ok = { status: 0, stdout: "", stderr: "" };
const failed = { status: 65, stdout: "", stderr: "ticket not yet available" };

function harness(results) {
  const calls = [];
  const waits = [];
  const logs = [];
  const invoke = (args, capture) => {
    calls.push({ args, capture });
    const result = results.shift();
    assert.ok(result, "unexpected xcrun call");
    return result;
  };
  const delay = async (ms) => waits.push(ms);
  return { calls, waits, logs, invoke, delay, record: (entry) => logs.push(entry) };
}

test("successful staple validates once before packaging", async () => {
  const h = harness([ok, ok]);
  assert.equal(await runXcrun(stapleArgs, h), 0);
  assert.deepEqual(h.calls.map((call) => call.args), [stapleArgs, validateArgs]);
  assert.deepEqual(h.waits, []);
});

test("retries two stapling failures and succeeds on the third attempt", async () => {
  const h = harness([failed, failed, ok, ok]);
  assert.equal(await runXcrun(stapleArgs, h), 0);
  assert.deepEqual(h.calls.map((call) => call.args), [
    stapleArgs, stapleArgs, stapleArgs, validateArgs,
  ]);
  assert.deepEqual(h.waits, [5000, 15000]);
  assert.ok(h.logs.some((message) => message.includes("exit status: 65")));
});

test("fails when all three stapling attempts fail", async () => {
  const h = harness([failed, failed, failed]);
  assert.equal(await runXcrun(stapleArgs, h), 1);
  assert.equal(h.calls.length, 3);
  assert.deepEqual(h.waits, [5000, 15000]);
  assert.ok(h.logs.at(-1).includes("failed after 3 attempts"));
});

test("a zero stapler exit code without a valid ticket is not success", async () => {
  const h = harness([ok, failed, ok, failed, ok, failed]);
  assert.equal(await runXcrun(stapleArgs, h), 1);
  assert.deepEqual(h.calls.map((call) => call.args), [
    stapleArgs, validateArgs, stapleArgs, validateArgs, stapleArgs, validateArgs,
  ]);
  assert.equal(h.waits.length, 2);
});

test("passes through all non-stapling xcrun commands without capturing output", async () => {
  const cmd = ["notarytool", "submit", "package.zip", "--wait"];
  const h = harness([{ status: 27 }]);
  assert.equal(await runXcrun(cmd, h), 27);
  assert.deepEqual(h.calls, [{ args: cmd, capture: false }]);
  assert.deepEqual(h.logs, []);
});

test("does not confuse stapler validate with a stapler staple request", async () => {
  const h = harness([ok]);
  assert.equal(await runXcrun(validateArgs, h), 0);
  assert.deepEqual(h.calls, [{ args: validateArgs, capture: false }]);
});

test("refuses a staple request with no artifact argument", async () => {
  const h = harness([]);
  assert.equal(await runXcrun(["stapler", "staple", "-v"], h), 1);
  assert.deepEqual(h.calls, []);
});

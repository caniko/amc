import assert from "node:assert/strict";
import test from "node:test";
import plugin, { memoryBytes, resourceCommand } from "./amc-resources-v2/server.js";

const resources = { memory_max: "512MiB", runtime_max_sec: 5, burst: true, reason: "small inspection" };

test("resource schema is mandatory and hooks preserve exact shell input", async () => {
  // The core shell keeps its typed executable schema. Session hooks receive
  // a distinct model-facing JSON schema; execute.before precedes core decoding.
  const inputSchema = { type: "object", properties: { command: { type: "string" } }, required: ["command"] };
  const hooks = new Map();
  let before;
  const executable = { input: "native schema", options: { custom: true } };
  await plugin.setup({ session: {
    async hook(name, fn) { hooks.set(name, fn); },
  }, tool: {
    async transform(fn) {
      fn({ update(name, update) { assert.equal(name, "shell"); update(executable); } });
    },
    async hook(name, fn) { assert.equal(name, "execute.before"); before = fn; },
  } });
  assert.equal(executable.input, "native schema");
  assert.deepEqual(executable.options, { custom: true, codemode: false });
  for (const kind of ["context", "compaction", "generate", "context"]) {
    // A rebuilt/reconnected session receives fresh native schema objects.
    const tool = { input: inputSchema };
    hooks.get(kind)({ tools: { shell: tool } });
    assert.deepEqual(tool.input.required, ["command"]);
    assert.deepEqual(tool.input.properties.command.required, ["text", "resources"]);
    assert.equal(tool.input.properties.command.properties.text.type, "string");
    const declared = tool.input;
    hooks.get(kind)({ tools: { shell: tool } });
    assert.equal(tool.input, declared, "schema declaration must be idempotent");
    hooks.get(kind)({ tools: {} });
  }
  assert.deepEqual(inputSchema.required, ["command"]);
  const script = "printf '%s' '$HOME';\nexit 7";
  const event = { tool: "shell", input: { command: { text: script, resources }, timeout: 1234 } };
  before(event);
  assert.equal(event.input.command.split("\n").slice(1).join("\n"), script);
  assert.equal(event.input.timeout, 1234);
  assert.equal(JSON.parse(event.input.command.split("\n")[0].slice("# canix-amc-resources-v1 ".length)).memory_max, 536870912);
  assert.equal("resources" in event.input, false);
  const raw = { tool: "shell", input: JSON.stringify({ command: { text: script, resources } }) };
  before(raw);
  assert.equal(raw.input.command, event.input.command);
  assert.throws(() => before({ tool: "shell", input: "{invalid" }));
  assert.throws(() => before({ tool: "shell", input: { command: "true" } }));
  const read = { tool: "read", input: { path: "file" } };
  before(read);
  assert.deepEqual(read.input, { path: "file" });
});

test("units and invalid or forged declarations cannot expand a grant", () => {
  assert.equal(memoryBytes("512MiB"), 536870912);
  assert.equal(memoryBytes("3GB"), 2999996416);
  for (const value of ["0B", "-1GiB", "1.5GiB", "10", "999999999999999999999GiB"]) {
    assert.throws(() => memoryBytes(value));
  }
  for (const override of [{ burst: true, runtime_max_sec: 31 }, { reason: "" },
    { reason: "hidden\ncommand" }, { runtime_max_sec: 1.5 }, { release: true }]) {
    assert.throws(() => resourceCommand({ command: "true", resources: { ...resources, ...override } }));
  }
  assert.throws(() => resourceCommand({ command: "# canix-amc-resources-v1 {}\ntrue", resources }));
});

test("ordinary calls declare resources independently of burst requests", () => {
  const envelope = resourceCommand({ command: "true", resources: {
    ...resources, burst: false, runtime_max_sec: 60,
  } });
  const declaration = JSON.parse(envelope.split("\n")[0].slice("# canix-amc-resources-v1 ".length));
  assert.deepEqual(declaration, {
    memory_max: 536870912, runtime_max_sec: 60, burst: false, reason: "small inspection",
  });
});

test("native start framing is opt-in and limited to the sized managed shell route", async () => {
  let create;
  await plugin.setup({
    options: { nativeStartShell: "/packaged/opencode-tool-shell" },
    shell: { async hook(name, fn) { assert.equal(name, "create.before"); create = fn; } },
    tool: { async transform() {}, async hook() {} },
    session: { async hook() {} },
  });
  const command = resourceCommand({ command: "printf original", resources });
  const managed = { shell: "/packaged/opencode-tool-shell", command, env: { KEEP: "original" } };
  create(managed);
  assert.equal(managed.env.OPENCODE_SHELL_NOTIFY_START, "1");
  assert.equal(managed.env.KEEP, "original");
  assert.equal(managed.command, command);
  for (const event of [
    { shell: "/packaged/bash", command, env: {} },
    { shell: "/packaged/opencode-tool-shell", command: "true", env: {} },
  ]) {
    create(event);
    assert.equal(event.env.OPENCODE_SHELL_NOTIFY_START, undefined);
  }
});

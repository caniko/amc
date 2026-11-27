import assert from "node:assert/strict";
import test from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import plugin from "./amc-status-v2/server.js";

const report = { schema_version: 1, collection_ms: 0,
  user: { state: "legacy", error: null, snapshot: {} }, host: { state: "unavailable", error: "timeout", snapshot: null } };

async function fixture(body, run) {
  const dir = await mkdtemp(join(tmpdir(), "amc-status-"));
  const binary = join(dir, "amc");
  await writeFile(binary, `#!${process.execPath}\n${body}`, { mode: 0o700 });
  let tool;
  const setup = () => plugin.setup({ options: { amcBin: binary }, tool: {
    async transform(fn) { fn({ add(value) { tool = value; } }); },
  } });
  try { await setup(); await run(() => tool, setup, dir); }
  finally { await rm(dir, { recursive: true, force: true }); }
}

test("fixed argv, strict empty input, replayable registration and no shell dependency", async () => {
  await fixture(`if (JSON.stringify(process.argv.slice(2)) !== '["admission","diagnose"]') process.exit(4); console.log(${JSON.stringify(JSON.stringify(report))});`, async (get, setup) => {
    const tool = get();
    assert.equal(tool.name, "admission_status");
    assert.equal(tool.options.codemode, false);
    assert.equal(tool.input.additionalProperties, false);
    assert.deepEqual(JSON.parse((await tool.execute({}, {})).content), report);
    await assert.rejects(tool.execute({ command: "anything" }, {}));
    await setup();
    assert.equal(get().name, tool.name);
  });
});

test("concurrent callers coalesce and cancellation does not cancel another caller", async () => {
  await fixture(`const fs=require('node:fs'); fs.appendFileSync(process.argv[1]+'.calls','x'); setTimeout(()=>console.log(${JSON.stringify(JSON.stringify(report))}),100);`, async (get, _setup, dir) => {
    const abort = new AbortController();
    const first = get().execute({}, { signal: abort.signal });
    const second = get().execute({}, {});
    abort.abort();
    await assert.rejects(first);
    assert.deepEqual(JSON.parse((await second).content), report);
    const { readFile } = await import("node:fs/promises");
    assert.equal(await readFile(join(dir, "amc.calls"), "utf8"), "x");
  });
});

test("deadline, malformed data, output ceiling and helper failures remain explicit", async () => {
  for (const body of ["setInterval(()=>{},1000)", "console.log('bad-json')", "process.stdout.write('x'.repeat(70000))", "process.exit(2)"]) {
    await fixture(body, async get => {
      const start = performance.now();
      const result = JSON.parse((await get().execute({}, {})).content);
      assert.equal(result.user.state, "unavailable");
      assert.equal(result.host.state, "unavailable");
      assert.ok(performance.now() - start < 2500);
    });
  }
});

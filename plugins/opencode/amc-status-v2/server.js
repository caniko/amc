import { spawn } from "node:child_process";
import { isAbsolute } from "node:path";

// Module scope shares a single collection across location-scoped plugin setups.
let active;
const MAX_BYTES = 65536;
const states = new Set(["available", "stale", "legacy", "incompatible", "unavailable", "not_configured"]);
const failure = error => ({ schema_version: 1, collection_ms: null,
  user: { state: "unavailable", error, snapshot: null },
  host: { state: "unavailable", error, snapshot: null } });

function collect(binary, signal) {
  return new Promise(resolve => {
    const child = spawn(binary, ["admission", "diagnose"], {
      shell: false, stdio: ["ignore", "pipe", "ignore"],
    });
    let bytes = 0;
    const chunks = [];
    let error;
    const stop = reason => { error ??= reason; child.kill("SIGKILL"); };
    const cancel = () => stop("cancelled");
    signal.addEventListener("abort", cancel, { once: true });
    const timer = setTimeout(() => stop("diagnostic_deadline"), 1900);
    child.stdout.on("data", chunk => {
      bytes += chunk.length;
      if (bytes > MAX_BYTES) stop("response_too_large");
      else if (!error) chunks.push(chunk);
    });
    child.on("error", () => { error ??= "diagnostic_helper_unavailable"; });
    child.on("close", code => {
      clearTimeout(timer);
      signal.removeEventListener("abort", cancel);
      if (error || code !== 0) return resolve(failure(error ?? "diagnostic_helper_failed"));
      try {
        const result = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        if (result.schema_version !== 1 || ![result.user, result.host].every(source =>
          source && states.has(source.state) && (source.snapshot === null || typeof source.snapshot === "object"))) {
          return resolve(failure("incompatible_diagnostic_response"));
        }
        resolve(result);
      } catch { resolve(failure("invalid_diagnostic_response")); }
    });
    if (signal.aborted) cancel();
  });
}

async function status(binary, signal) {
  signal?.throwIfAborted();
  if (active && active.binary !== binary) return failure("diagnostic_collector_busy");
  if (!active) {
    const controller = new AbortController();
    const current = { binary, controller, users: 0 };
    current.promise = collect(binary, controller.signal).finally(() => {
      if (active === current) active = undefined;
    });
    active = current;
  }
  const current = active;
  current.users++;
  let cancel;
  try {
    return await Promise.race([current.promise, new Promise((_, reject) => {
      cancel = () => reject(signal.reason ?? new Error("cancelled"));
      signal?.addEventListener("abort", cancel, { once: true });
      if (signal?.aborted) cancel();
    })]);
  } finally {
    signal?.removeEventListener("abort", cancel);
    if (--current.users === 0) current.controller.abort();
  }
}

export default {
  id: "canix.amc-status",
  async setup(ctx) {
    const binary = ctx.options.amcBin;
    if (typeof binary !== "string" || !isAbsolute(binary)) throw new Error("AMC diagnostics requires an absolute packaged binary.");
    await ctx.tool.transform(editor => editor.add({
      name: "admission_status",
      description: "Read bounded user/host AMC diagnostics without shell admission. Explains queued work, observation freshness and compatibility. Layers overlap: never sum their commitments. Available means readable, not permission to execute. No mutation, retries or job cancellation.",
      input: { type: "object", properties: {}, additionalProperties: false },
      options: { codemode: false },
      execute: async (input, context) => {
        if (!input || typeof input !== "object" || Array.isArray(input) || Object.keys(input).length) {
          throw new Error("admission_status accepts only an empty object.");
        }
        return { content: JSON.stringify(await status(binary, context.signal)) };
      },
    }));
  },
};

const PREFIX = "# canix-amc-resources-v1 ";

export function memoryBytes(value) {
  const match = typeof value === "string" && value.length <= 64
    && /^(\d+)(B|KiB|MiB|GiB|KB|MB|GB)$/.exec(value);
  if (!match) throw new Error("Declare an integral memory ceiling with B, KiB, MiB, GiB, KB, MB or GB.");
  const factors = { B: 1n, KiB: 1024n, MiB: 1048576n, GiB: 1073741824n,
    KB: 1000n, MB: 1000000n, GB: 1000000000n };
  const bytes = BigInt(match[1]) * factors[match[2]];
  // Round down: kernel page rounding must never exceed the declaration.
  const aligned = bytes / 4096n * 4096n;
  if (aligned <= 0n || aligned > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("Memory ceiling is outside the supported range.");
  }
  return Number(aligned);
}

export function resourceCommand(input) {
  const resources = input?.resources;
  if (!resources || typeof input.command !== "string") {
    throw new Error("Every shell call needs resources: memory_max, runtime_max_sec, burst and reason.");
  }
  if (Object.keys(resources).sort().join(",") !== "burst,memory_max,reason,runtime_max_sec") {
    throw new Error("Supply exactly memory_max, runtime_max_sec, burst and reason.");
  }
  const { runtime_max_sec, burst, reason } = resources;
  if (!Number.isInteger(runtime_max_sec) || runtime_max_sec < 1 || runtime_max_sec > 86400
    || typeof burst !== "boolean" || (burst && runtime_max_sec > 30)
    || typeof reason !== "string" || !reason.trim() || Buffer.byteLength(reason) > 512
    || /[\u0000-\u001f\u007f-\u009f]/.test(reason)) {
    throw new Error("Invalid bounded resource declaration; burst calls need a short runtime.");
  }
  if (input.command.startsWith(PREFIX)) {
    throw new Error("Resource envelopes are harness-owned; supply the original command.");
  }
  const declaration = { memory_max: memoryBytes(resources.memory_max), runtime_max_sec, burst, reason };
  return `${PREFIX}${JSON.stringify(declaration)}\n${input.command}`;
}

export default {
  id: "canix.amc-resources",
  async setup(ctx) {
    // Opt in only when Nix selected a capable native adapter and Core runtime.
    // Direct/user shells and opt-out hosts must not await a nonexistent frame.
    if (ctx.options?.nativeStartShell) {
      const shell = ctx.options.nativeStartShell;
      if (typeof shell !== "string" || !shell.startsWith("/")) {
        throw new Error("Native start notification needs an absolute managed shell path.");
      }
      await ctx.shell.hook("create.before", event => {
        event.env.OPENCODE_SHELL_NOTIFY_START = event.shell === shell && event.command.startsWith(PREFIX) ? "1" : undefined;
      });
    }
    // Code Mode's catalog is built from the native schema before session hooks.
    // Keep shell calls on the directly exposed schema carrying the declaration.
    await ctx.tool.transform(editor => editor.update("shell", tool => {
      tool.options = { ...tool.options, codemode: false };
    }));
    // V2 repairs inputs against its native schema before external hooks, dropping
    // unknown top-level fields. Carry the declaration inside the existing command
    // field, then unwrap it before the unchanged executable schema decodes it.
    const declare = event => {
      const tool = event.tools.shell;
      if (!tool) return;
      const input = tool.input;
      if (input.properties.command.properties?.resources && input.properties.command.properties?.text) return;
      tool.input = { ...input, properties: { ...input.properties,
        command: { type: "object", additionalProperties: false,
          description: "Command text and an honest per-call resource declaration. The harness validates and unwraps this before execution.",
          properties: {
            text: input.properties.command,
            resources: { type: "object", additionalProperties: false,
              properties: {
                memory_max: { type: "string", maxLength: 64, description: "Honest hard RAM estimate for the command and descendants, including tool overhead; e.g. 256MiB. Separately owned daemon work needs its own admission." },
                runtime_max_sec: { type: "integer", minimum: 1, maximum: 86400,
                  description: "Native execution deadline; a short deadline alone does not establish burst eligibility." },
                burst: { type: "boolean", description: "Suggest bounded short-call burst admission. AMC policy and physical headroom decide eligibility." },
                reason: { type: "string", minLength: 1, maxLength: 512,
                  description: "Brief evidence or assumptions behind the resource estimate." },
              }, required: ["memory_max", "runtime_max_sec", "burst", "reason"] },
          }, required: ["text", "resources"] },
      } };
    };
    for (const kind of ["context", "compaction", "generate"]) {
      await ctx.session.hook(kind, declare);
    }
    await ctx.tool.hook("execute.before", event => {
      if (event.tool !== "shell") return;
      const decoded = typeof event.input === "string" ? JSON.parse(event.input) : event.input;
      const request = decoded?.command;
      if (!request || typeof request !== "object" || Array.isArray(request)
        || Object.keys(request).sort().join(",") !== "resources,text") {
        throw new Error("Every shell call needs command: { text, resources }. Resources must declare memory_max, runtime_max_sec, burst and reason.");
      }
      event.input = { ...decoded, command: resourceCommand({ command: request.text, resources: request.resources }) };
    });
  },
};

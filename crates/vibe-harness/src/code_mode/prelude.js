// The sandbox one `run_typescript` evaluation runs in, written for this port.
//
// The host substitutes two JSON string literals before compiling it: the
// program, already transpiled, and the evaluation's own facts (the operations
// recorded so far, the tool namespaces, the clock, the random seed and the
// budgets). A program runs from the start on every evaluation: a tool call
// the record answers settles at once, and the first one it does not answer
// suspends the program. Every unanswered call made before the suspension
// lands is reported together, and the host runs them before evaluating again.
(function () {
  "use strict";
  const facts = JSON.parse(__VIBE_FACTS__);
  const source = __VIBE_PROGRAM__;
  const stdout = [];
  const stderr = [];
  const pending = [];
  let outcome = null;
  let calls = 0;
  let suspending = false;
  let suspended = false;

  const show = (value) => {
    if (typeof value === "string") return value;
    try {
      const text = JSON.stringify(value);
      return text === undefined ? String(value) : text;
    } catch (_) {
      return String(value);
    }
  };
  const line = (values) => values.map(show).join(" ");
  const sandboxConsole = Object.freeze({
    log: (...values) => { stdout.push(line(values)); },
    info: (...values) => { stdout.push(line(values)); },
    debug: (...values) => { stdout.push(line(values)); },
    error: (...values) => { stderr.push(line(values)); },
    warn: (...values) => { stderr.push(line(values)); },
  });

  const canonical = (value) => {
    if (Array.isArray(value)) return "[" + value.map(canonical).join(",") + "]";
    if (value !== null && typeof value === "object") {
      return "{" + Object.keys(value).sort()
        .filter((key) => value[key] !== undefined)
        .map((key) => JSON.stringify(key) + ":" + canonical(value[key]))
        .join(",") + "}";
    }
    const text = JSON.stringify(value);
    return text === undefined ? "null" : text;
  };
  const plain = (value) => {
    const text = JSON.stringify(value === undefined ? {} : value);
    return text === undefined ? {} : JSON.parse(text);
  };
  // A recorded failure with a message comes back as an Error carrying the
  // record as its cause; any other rejection value is thrown as it is.
  const rebuilt = (recorded) => {
    if (!recorded || typeof recorded !== "object" || typeof recorded.message !== "string") return recorded;
    const error = new Error(recorded.message, { cause: recorded });
    error.name = typeof recorded.name === "string" ? recorded.name : "Error";
    if (typeof recorded.stack === "string") error.stack = recorded.stack;
    return error;
  };

  const call = (name, argument) => {
    const index = calls;
    calls += 1;
    if (index >= facts.maxEffects) {
      throw new Error(`run_typescript exceeded maxProgramEffects (${facts.maxEffects}).`);
    }
    const sent = plain(argument);
    const recorded = facts.record[index];
    if (recorded && recorded.name === name && canonical(recorded.arguments) === canonical(sent)) {
      return recorded.ok ? Promise.resolve(recorded.value) : Promise.reject(rebuilt(recorded.error));
    }
    if (!suspended) {
      pending.push({ name, arguments: sent });
      if (!suspending) {
        suspending = true;
        Promise.resolve().then(() => { suspended = true; });
      }
    }
    return new Promise(() => {});
  };

  const tools = {};
  for (const [namespace, functions] of Object.entries(facts.tools)) {
    const group = {};
    for (const name of functions) {
      group[name] = (argument) => call(`${namespace}.${name}`, argument);
    }
    tools[namespace] = Object.freeze(group);
  }
  Object.freeze(tools);

  // A program sees one clock and one random sequence on every evaluation,
  // so replaying it takes the same branches.
  let seed = facts.seed >>> 0 || 0x9e3779b9;
  const random = () => {
    seed ^= seed << 13; seed >>>= 0;
    seed ^= seed >>> 17;
    seed ^= seed << 5; seed >>>= 0;
    return seed / 4294967296;
  };
  const RealDate = Date;
  const now = facts.now;
  function SandboxDate(...values) {
    if (!new.target) return new RealDate(now).toString();
    return values.length === 0 ? new RealDate(now) : new RealDate(...values);
  }
  SandboxDate.prototype = RealDate.prototype;
  SandboxDate.now = () => now;
  SandboxDate.parse = RealDate.parse;
  SandboxDate.UTC = RealDate.UTC;
  globalThis.Date = SandboxDate;
  Math.random = random;
  const hex = (count) => Array.from({ length: count }, () => Math.floor(random() * 16).toString(16)).join("");
  globalThis.crypto = Object.freeze({
    randomUUID: () => `${hex(8)}-${hex(4)}-4${hex(3)}-${(8 + Math.floor(random() * 4)).toString(16)}${hex(3)}-${hex(12)}`,
    getRandomValues: (array) => {
      for (let index = 0; index < array.length; index += 1) {
        array[index] = Math.floor(random() * 256);
      }
      return array;
    },
  });
  globalThis.performance = Object.freeze({ now: () => 0 });
  globalThis.fetch = () => {
    throw new Error("fetch() is not supported in the sandbox. Use a tool to make HTTP requests instead.");
  };
  globalThis.setTimeout = (callback, delay, ...values) => {
    if (delay !== undefined && delay !== 0) {
      throw new Error("setTimeout() only supports a delay of 0 in the sandbox.");
    }
    queueMicrotask(() => callback(...values));
    return 0;
  };
  globalThis.clearTimeout = () => {};
  globalThis.setImmediate = (callback, ...values) => {
    queueMicrotask(() => callback(...values));
    return 0;
  };
  globalThis.setInterval = () => {
    throw new Error("setInterval() is not supported in the sandbox.");
  };
  globalThis.clearInterval = () => {
    throw new Error("clearInterval() is not supported in the sandbox.");
  };
  globalThis.eval = () => {
    throw new Error("eval() is not supported in the sandbox.");
  };
  for (const name of ["Deno", "__bootstrap"]) {
    try { delete globalThis[name]; } catch (_) { /* not configurable */ }
  }

  const step = (callback) => callback();
  const described = (error) => {
    try {
      if (error instanceof Error) {
        const report = { name: error.name, message: error.message };
        if (typeof error.stack === "string") report.stack = error.stack;
        return report;
      }
      const copy = JSON.parse(JSON.stringify(error === undefined ? null : error));
      return copy === undefined ? { name: "Error", message: String(error) } : copy;
    } catch (_) {
      return { name: "Error", message: String(error) };
    }
  };
  const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/;
  const serialized = (value) => {
    const text = JSON.stringify(value === undefined ? null : value);
    if (text === undefined) return "null";
    if (LONE_SURROGATE.test(text)) {
      const error = new Error("run_typescript result contains an unpaired surrogate");
      error.name = "SerializationError";
      throw error;
    }
    return text;
  };

  const AsyncFunction = (async function () {}).constructor;
  try {
    const program = new AsyncFunction(
      "tools",
      "step",
      "input",
      "console",
      // The program starts on the body's second line and the check follows
      // it on a line of its own, so stack positions read as the reference's.
      "\n" + source +
        "\nif (typeof main !== \"function\") throw new Error(\"run_typescript code must define async function main().\");\nreturn await main(input);",
    );
    program(tools, step, {}, sandboxConsole).then(
      (value) => {
        try {
          outcome = { status: "returned", value: serialized(value) };
        } catch (error) {
          outcome = { status: "threw", error: described(error) };
        }
      },
      (error) => { outcome = { status: "threw", error: described(error) }; },
    );
  } catch (error) {
    outcome = { status: "threw", error: described(error) };
  }

  globalThis.__vibeCollect = () => JSON.stringify({
    pending,
    calls,
    stdout: stdout.join("\n"),
    stderr: stderr.join("\n"),
    outcome,
  });
})();

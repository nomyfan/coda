(() => {
  const callTool = globalThis.__coda_call_tool;
  const appendLog = globalThis.__coda_log;
  const toolNames = JSON.parse(globalThis.__coda_tool_names);
  Reflect.deleteProperty(globalThis, "__coda_call_tool");
  Reflect.deleteProperty(globalThis, "__coda_log");
  Reflect.deleteProperty(globalThis, "__coda_tool_names");

  const consoleObject = Object.freeze({
    log: (...values) => {
      const line = values
        .map((value) => {
          if (typeof value === "string") return value;
          try {
            const encoded = JSON.stringify(value);
            return encoded === undefined ? String(value) : encoded;
          } catch (_) {
            return String(value);
          }
        })
        .join(" ");
      const text = line + "\n";
      // At most 4096 UTF-16 code units => at most 16384 UTF-8 bytes.
      // Keep surrogate pairs together before crossing the native boundary.
      for (let start = 0; start < text.length; ) {
        let end = Math.min(start + 4096, text.length);
        const last = text.charCodeAt(end - 1);
        if (end < text.length && last >= 0xd800 && last <= 0xdbff) --end;
        appendLog(text.slice(start, end));
        start = end;
      }
    },
  });
  Object.defineProperty(globalThis, "console", {
    value: consoleObject,
    enumerable: true,
    configurable: false,
    writable: false,
  });

  class ToolError extends Error {
    constructor(code, message) {
      super(message);
      this.name = "ToolError";
      this.code = code;
      Object.defineProperty(this, "message", { enumerable: true });
    }
  }

  const toolsObject = Object.create(null);
  for (const name of toolNames) {
    Object.defineProperty(toolsObject, name, {
      enumerable: true,
      configurable: false,
      writable: false,
      value: async (input) => {
        if (input === null || typeof input !== "object" || Array.isArray(input)) {
          throw new TypeError(`${name} expects one object argument`);
        }
        const [ok, value, message] = await callTool(name, JSON.stringify(input));
        if (!ok) throw new ToolError(value, message);
        return value;
      },
    });
  }
  Object.freeze(toolsObject);
  Object.defineProperty(globalThis, "tools", {
    value: toolsObject,
    enumerable: true,
    configurable: false,
    writable: false,
  });
})();

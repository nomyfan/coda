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
      appendLog(line);
    },
  });
  Object.defineProperty(globalThis, "console", {
    value: consoleObject,
    enumerable: true,
    configurable: false,
    writable: false,
  });

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
        return await callTool(name, JSON.stringify(input));
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

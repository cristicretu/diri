// Diri's sidebar Browser: keeps the page's recent console messages and
// uncaught errors for Agents (`sidebar_browser` console). Page world,
// document start, main frame; bounded to the newest 200 entries.
(() => {
  if (window.__diriConsole) return;
  const entries = [];
  let seq = 0;
  const keep = (level, parts) => {
    const text = parts
      .map((part) => {
        if (part instanceof Error) return `${part.name}: ${part.message}`;
        if (typeof part === "string") return part;
        try { return JSON.stringify(part); } catch { return String(part); }
      })
      .join(" ")
      .slice(0, 2000);
    entries.push({ seq: ++seq, level, text, at: Date.now() });
    if (entries.length > 200) entries.shift();
  };
  for (const level of ["log", "info", "warn", "error", "debug"]) {
    const original = console[level];
    console[level] = function (...parts) {
      keep(level, parts);
      return original.apply(this, parts);
    };
  }
  addEventListener("error", (event) => {
    const where = event.filename ? ` (${event.filename}:${event.lineno})` : "";
    keep("uncaught", [`${event.message}${where}`]);
  });
  addEventListener("unhandledrejection", (event) => keep("unhandledrejection", [event.reason]));
  Object.defineProperty(window, "__diriConsole", { value: { entries, read: 0 }, enumerable: false });
})();

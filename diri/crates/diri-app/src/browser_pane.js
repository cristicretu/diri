// Diri's sidebar Browser helper for Agents. Runs in WebKit's isolated client
// world: it shares the page's DOM but none of its globals, so the page cannot
// see or tamper with refs or the cursor. Installed once per page.
if (!window.__diri) {
  const MAX_LINES = 600;
  const MAX_LINE = 240;
  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "HEAD", "META", "LINK"]);
  const INTERACTIVE = new Set([
    "link", "button", "textbox", "searchbox", "checkbox", "radio", "slider", "combobox",
    "listbox", "spinbutton", "file", "tab", "menuitem", "switch", "option", "clickable",
  ]);
  const LANDMARK = new Set(["form", "navigation", "main", "dialog", "list", "table", "region", "banner", "contentinfo"]);
  const refs = new Map();
  const ids = new WeakMap();
  let next = 1;

  const clip = (text, max) => {
    const flat = String(text ?? "").replace(/\s+/g, " ").trim();
    return flat.length > max ? flat.slice(0, max - 1) + "…" : flat;
  };
  const refOf = (el) => {
    let ref = ids.get(el);
    if (!ref) {
      ref = "e" + next++;
      ids.set(el, ref);
      refs.set(ref, new WeakRef(el));
    }
    return ref;
  };
  const find = (ref) => {
    const el = refs.get(String(ref))?.deref();
    if (!el || !el.isConnected) {
      throw new Error(`stale or unknown ref ${ref}: take a new snapshot`);
    }
    return el;
  };
  const rendered = (el) => {
    const style = getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return false;
    if (el.getAttribute("aria-hidden") === "true") return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 || rect.height > 0 || style.display === "contents";
  };
  const roleOf = (el) => {
    const explicit = el.getAttribute("role");
    if (explicit) return explicit.split(/\s+/)[0];
    const tag = el.tagName.toLowerCase();
    switch (tag) {
      case "a": return el.hasAttribute("href") ? "link" : null;
      case "button": case "summary": return "button";
      case "select": return el.multiple ? "listbox" : "combobox";
      case "textarea": return "textbox";
      case "option": return "option";
      case "img": return "img";
      case "nav": return "navigation";
      case "main": return "main";
      case "form": return "form";
      case "dialog": return "dialog";
      case "table": return "table";
      case "ul": case "ol": return "list";
      case "li": return "listitem";
      case "h1": case "h2": case "h3": case "h4": case "h5": case "h6": return "heading";
      case "input": {
        const type = (el.getAttribute("type") || "text").toLowerCase();
        return {
          checkbox: "checkbox", radio: "radio", button: "button", submit: "button", reset: "button",
          image: "button", range: "slider", file: "file", hidden: null, search: "searchbox",
          number: "spinbutton",
        }[type] ?? (type === "hidden" ? null : "textbox");
      }
    }
    if (el.isContentEditable && el.getAttribute("contenteditable") !== null) return "textbox";
    if (getComputedStyle(el).cursor === "pointer" && (el.onclick || el.hasAttribute("onclick") || el.tabIndex >= 0)) {
      return "clickable";
    }
    return null;
  };
  const nameOf = (el, role) => {
    const aria = el.getAttribute("aria-label");
    if (aria) return aria;
    const labelledBy = el.getAttribute("aria-labelledby");
    if (labelledBy) {
      const text = labelledBy.split(/\s+/).map((id) => document.getElementById(id)?.innerText || "").join(" ");
      if (text.trim()) return text;
    }
    if (el.labels && el.labels.length) return [...el.labels].map((label) => label.innerText).join(" ");
    if (el.tagName === "IMG") return el.getAttribute("alt") || "";
    if (["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName)) {
      if (el.type === "submit" || el.type === "button" || el.type === "reset") return el.value || el.type;
      return el.getAttribute("placeholder") || el.getAttribute("title") || el.getAttribute("name") || "";
    }
    if (role === "heading" || INTERACTIVE.has(role)) return el.innerText || el.textContent || el.getAttribute("title") || "";
    return el.getAttribute("title") || "";
  };
  const ownText = (el) => {
    let text = "";
    for (const node of el.childNodes) {
      if (node.nodeType === Node.TEXT_NODE) text += node.textContent;
    }
    return text.replace(/\s+/g, " ").trim();
  };
  const describe = (el, role) => {
    const ref = refOf(el);
    let line = `${role} "${clip(nameOf(el, role), 100)}" [${ref}]`;
    if (role === "heading") line = `heading${el.tagName.slice(1)} "${clip(nameOf(el, role), 140)}" [${ref}]`;
    if (["textbox", "searchbox", "spinbutton", "combobox", "slider"].includes(role)) {
      const value = el.isContentEditable ? el.innerText : el.value;
      if (value) line += ` = "${clip(el.type === "password" ? "•".repeat(Math.min(String(value).length, 8)) : value, 80)}"`;
    }
    if ((role === "checkbox" || role === "radio" || role === "switch") && (el.checked || el.getAttribute("aria-checked") === "true")) line += " (checked)";
    if (el.disabled || el.getAttribute("aria-disabled") === "true") line += " (disabled)";
    if (el.required) line += " (required)";
    if (el.getAttribute("aria-invalid") === "true" || (el.validity && !el.validity.valid && el.value)) line += " (invalid)";
    if (document.activeElement === el) line += " (focused)";
    if (role === "link") {
      const href = el.getAttribute("href");
      if (href && !href.startsWith("javascript:")) line += ` -> ${clip(href, 80)}`;
    }
    const rect = el.getBoundingClientRect();
    if (rect.bottom < 0 || rect.top > innerHeight || rect.right < 0 || rect.left > innerWidth) line += " (offscreen)";
    return line;
  };

  const snapshot = () => {
    const lines = [];
    let truncated = false;
    const emit = (depth, text) => {
      if (lines.length >= MAX_LINES) {
        truncated = true;
        return false;
      }
      lines.push("  ".repeat(depth) + "- " + clip(text, MAX_LINE));
      return true;
    };
    const walk = (el, depth) => {
      if (truncated || SKIP.has(el.tagName) || el.id === "diri-agent-cursor") return;
      if (!rendered(el)) return;
      const role = roleOf(el);
      if (role && (INTERACTIVE.has(role) || role === "heading" || role === "img")) {
        if (role === "img" && !nameOf(el, role)) return;
        emit(depth, describe(el, role));
        if (role === "combobox" || role === "listbox") {
          for (const option of el.querySelectorAll?.("option") ?? []) {
            emit(depth + 1, `option "${clip(option.textContent, 80)}"${option.selected ? " (selected)" : ""}`);
          }
        }
        return;
      }
      let childDepth = depth;
      if (role && LANDMARK.has(role)) {
        const name = nameOf(el, role);
        emit(depth, name ? `${role} "${clip(name, 80)}"` : role);
        childDepth = depth + 1;
      }
      const text = ownText(el);
      if (text) emit(childDepth, `text "${clip(text, 200)}"`);
      const children = el.shadowRoot ? [...el.shadowRoot.children, ...el.children] : el.children;
      for (const child of children) walk(child, childDepth);
    };
    if (document.body) walk(document.body, 0);
    return {
      url: location.href,
      title: document.title,
      outline: lines.join("\n"),
      truncated,
      note: truncated ? `Outline cut at ${MAX_LINES} lines; scroll or use eval for the rest.` : undefined,
    };
  };

  // The agent cursor: a pointer with the Agent's badge that glides to the
  // element it is about to act on. Decorative; it never takes pointer events.
  let cursor = null;
  let hideTimer = 0;
  const cursorAt = (badge) => {
    if (!cursor || !cursor.host.isConnected) {
      const host = document.createElement("div");
      host.id = "diri-agent-cursor";
      host.style.cssText = "position:fixed;left:0;top:0;width:0;height:0;z-index:2147483647;pointer-events:none;";
      const root = host.attachShadow({ mode: "closed" });
      root.innerHTML = `<style>
        .c{position:fixed;left:0;top:0;opacity:0;transition:opacity .25s;pointer-events:none;will-change:transform}
        .c.on{opacity:1}
        svg{display:block;filter:drop-shadow(0 1px 2px rgba(0,0,0,.35))}
        .b{position:absolute;left:16px;top:18px;white-space:nowrap;font:600 11px/1.2 -apple-system,system-ui,sans-serif;color:#fff;background:#ff6a3d;padding:3px 7px;border-radius:7px;box-shadow:0 1px 3px rgba(0,0,0,.3)}
        .r{position:absolute;left:-14px;top:-14px;width:28px;height:28px;border-radius:50%;border:2px solid #ff6a3d;opacity:0;transform:scale(.4)}
        .r.go{animation:p .5s ease-out}
        @keyframes p{0%{opacity:.9;transform:scale(.4)}100%{opacity:0;transform:scale(1.6)}}
        @media (prefers-reduced-motion: reduce){.r.go{animation:none}}
      </style><div class="c"><div class="r"></div><svg width="18" height="22" viewBox="0 0 18 22"><path d="M1 1l15 9.5-6.6 1.4 3.8 7.6-2.9 1.4-3.8-7.6L1 18z" fill="#ff6a3d" stroke="#fff" stroke-width="1.5" stroke-linejoin="round"/></svg><div class="b"></div></div>`;
      (document.body || document.documentElement).appendChild(host);
      const pointer = root.querySelector(".c");
      cursor = { host, pointer, ripple: root.querySelector(".r"), badge: root.querySelector(".b"), x: innerWidth / 2, y: innerHeight / 2, glide: 0 };
      place(cursor, cursor.x, cursor.y);
    }
    cursor.badge.textContent = badge || "Agent";
    return cursor;
  };
  const place = (c, x, y) => {
    c.x = x;
    c.y = y;
    c.pointer.style.transform = `translate(${x}px, ${y}px)`;
  };
  // Timer-driven, not a CSS transition: WebKit suspends transitions in a
  // panel it considers hidden, and the cursor must still land.
  const GLIDE_MS = 300;
  const point = (el, badge) => {
    el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    const rect = el.getBoundingClientRect();
    const x = Math.round(rect.left + Math.min(rect.width / 2, 40));
    const y = Math.round(rect.top + rect.height / 2);
    const c = cursorAt(badge);
    c.pointer.classList.add("on");
    clearTimeout(c.glide);
    const fromX = c.x;
    const fromY = c.y;
    const start = Date.now();
    const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
    const step = () => {
      const t = still ? 1 : Math.min(1, (Date.now() - start) / GLIDE_MS);
      const eased = 1 - Math.pow(1 - t, 3);
      place(c, fromX + (x - fromX) * eased, fromY + (y - fromY) * eased);
      if (t < 1) c.glide = setTimeout(step, 16);
    };
    step();
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => c.pointer.classList.remove("on"), 2500);
    return { x, y };
  };
  const pulse = () => {
    if (!cursor) return;
    cursor.ripple.classList.remove("go");
    void cursor.ripple.offsetWidth;
    cursor.ripple.classList.add("go");
  };

  const mouse = (el, type, x, y) => {
    const init = { bubbles: true, cancelable: true, composed: true, clientX: x, clientY: y, button: 0, view: window };
    const Ctor = type.startsWith("pointer") ? PointerEvent : MouseEvent;
    return el.dispatchEvent(new Ctor(type, { ...init, pointerId: 1, pointerType: "mouse", isPrimary: true }));
  };
  const click = (el) => {
    const rect = el.getBoundingClientRect();
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    const target = document.elementFromPoint(x, y);
    const hit = target && (target === el || el.contains(target)) ? target : el;
    mouse(hit, "pointerover", x, y);
    mouse(hit, "pointerdown", x, y);
    mouse(hit, "mousedown", x, y);
    if (typeof el.focus === "function") el.focus({ preventScroll: true });
    mouse(hit, "pointerup", x, y);
    mouse(hit, "mouseup", x, y);
    if (typeof hit.click === "function") hit.click();
    else mouse(hit, "click", x, y);
    pulse();
    return { clicked: refOf(el) };
  };
  const nativeSetter = (el) => {
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
      : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
    return Object.getOwnPropertyDescriptor(proto, "value")?.set;
  };
  const fill = (el, value) => {
    if (typeof el.focus === "function") el.focus({ preventScroll: true });
    if (el.type === "checkbox" || el.type === "radio") {
      const want = !["false", "0", "off", "no", ""].includes(String(value).toLowerCase());
      if (el.checked !== want) click(el);
      return { checked: el.checked };
    }
    if (el.isContentEditable) {
      el.textContent = value;
      el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
      pulse();
      return { value: el.innerText };
    }
    if (el instanceof HTMLSelectElement) {
      // Agents name an option by what the person sees as often as by value.
      const wanted = String(value).trim().toLowerCase();
      const option = [...el.options].find((o) => o.value === String(value))
        || [...el.options].find((o) => o.textContent.trim().toLowerCase() === wanted);
      if (!option) {
        const choices = [...el.options].map((o) => `"${clip(o.textContent, 40)}"`).join(", ");
        throw new Error(`no option ${JSON.stringify(String(value))}; choose one of ${choices}`);
      }
      value = option.value;
    }
    const setter = nativeSetter(el);
    if (!setter) throw new Error("that element does not take a value");
    setter.call(el, value);
    el.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, inputType: "insertText", data: value }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    pulse();
    return { value: el.type === "password" ? "•".repeat(Math.min(String(el.value).length, 8)) : el.value };
  };
  const KEY_CODES = { Enter: "Enter", Tab: "Tab", Escape: "Escape", Backspace: "Backspace", " ": "Space", ArrowDown: "ArrowDown", ArrowUp: "ArrowUp", ArrowLeft: "ArrowLeft", ArrowRight: "ArrowRight" };
  const press = (key) => {
    const el = document.activeElement || document.body;
    const init = { key, code: KEY_CODES[key] || (key.length === 1 ? `Key${key.toUpperCase()}` : key), bubbles: true, cancelable: true, composed: true };
    const proceed = el.dispatchEvent(new KeyboardEvent("keydown", init));
    if (proceed && key.length === 1) el.dispatchEvent(new KeyboardEvent("keypress", init));
    if (proceed) {
      if (key === "Enter" && el.form && el.tagName === "INPUT") el.form.requestSubmit();
      else if (key === "Enter" && (el.tagName === "BUTTON" || el.tagName === "A")) el.click();
      else if (key === "Tab") {
        const focusable = [...document.querySelectorAll("a[href],button,input,select,textarea,[tabindex]:not([tabindex='-1'])")]
          .filter((node) => !node.disabled && rendered(node));
        const at = focusable.indexOf(el);
        focusable[(at + 1) % focusable.length]?.focus();
      } else if (key.length === 1 && ("value" in el) && typeof el.setRangeText === "function") {
        el.setRangeText(key, el.selectionStart ?? el.value.length, el.selectionEnd ?? el.value.length, "end");
        el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: key }));
      }
    }
    el.dispatchEvent(new KeyboardEvent("keyup", init));
    const focused = document.activeElement;
    return { key, focused: focused && focused !== document.body ? refOf(focused) : null };
  };

  window.__diri = {
    point(ref, badge) {
      return point(find(ref), badge);
    },
    run(action, args) {
      switch (action) {
        case "snapshot": return snapshot();
        case "click": return click(find(args.ref));
        case "fill": return fill(find(args.ref), String(args.value ?? ""));
        case "press": return press(String(args.key || "Enter"));
        case "scroll":
          if (args.ref) find(args.ref).scrollIntoView({ block: "center", behavior: "instant" });
          else window.scrollBy({ top: Number(args.dy || innerHeight * 0.8), behavior: "instant" });
          return { scrollY: Math.round(scrollY), height: document.documentElement.scrollHeight };
        case "find_text":
          return { found: (document.body?.innerText || "").includes(String(args.text)) };
        default: throw new Error(`unknown action ${action}`);
      }
    },
  };
}

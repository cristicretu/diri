// Headless browser half of `diri-web`'s browser terminal test. The Rust test
// (`browser_terminal` in src/lib.rs) owns the fake Engine and the gateway and
// runs this script once per browser; the two talk over stdio: this script
// prints `need <step>` and waits for `ok` (or `fail …`) on stdin.
//
//   node terminal.mjs <base-url> <token> <chromium|webkit>
//
// DIRI_WEB_PLAYWRIGHT is the path of an installed `playwright-core` package
// whose browsers are installed. Nothing is shown on screen: headless only.

import { pathToFileURL } from "node:url";
import { createInterface } from "node:readline";

const [base, token, which] = process.argv.slice(2);
const playwright = await import(pathToFileURL(process.env.DIRI_WEB_PLAYWRIGHT + "/index.mjs").href);

const replies = createInterface({ input: process.stdin })[Symbol.asyncIterator]();
async function need(step) {
  process.stdout.write("need " + step + "\n");
  const { value } = await replies.next();
  if (value !== "ok") throw new Error(step + ": " + value);
}

function check(condition, message) {
  if (!condition) throw new Error(message);
}

const browser = await playwright[which].launch({ headless: true });
let failed = null;
try {
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    deviceScaleFactor: 2,
    isMobile: which === "chromium",
    hasTouch: true,
  });
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(String(error)));

  // The enrolment link trades the token for a cookie, then a session link
  // opens the terminal directly.
  await page.goto(base + "/?token=" + token);
  await page.goto(base + "/?open=1#s=s_a");
  await need("seed");
  const live = (text) => page.waitForFunction(
    (wanted) => (document.querySelector(".term-live") || {}).textContent?.includes(wanted),
    text, { timeout: 10000 });
  await live("hello browser");

  // 40 columns fit a phone at a readable size, with colour from the palette.
  const look = await page.evaluate(() => {
    const term = document.querySelector(".term");
    const span = document.querySelector(".term-live .term-row span");
    return {
      font: parseFloat(getComputedStyle(term).getPropertyValue("--term-font")),
      color: getComputedStyle(span).color,
      weight: getComputedStyle(span).fontWeight,
      overflow: document.querySelector(".term-viewport").scrollWidth -
        document.querySelector(".term-viewport").clientWidth,
    };
  });
  check(look.font >= 12, "font too small for 40 columns: " + look.font);
  check(look.overflow <= 1, "40 columns overflow the phone: " + look.overflow);
  check(look.color === "rgb(205, 49, 49)", "ANSI red not applied: " + look.color);
  check(Number(look.weight) >= 600, "bold not applied: " + look.weight);

  // Key row and typing: Esc, sticky Ctrl then c, Up, then text and Enter.
  await page.tap(".term-viewport");
  await page.tap('.key[data-key="esc"]');
  await page.tap('.key[data-key="ctrl"]');
  check(await page.$eval('.key[data-key="ctrl"]', (k) => k.classList.contains("armed")), "ctrl not armed");
  await page.keyboard.type("c");
  check(!(await page.$eval('.key[data-key="ctrl"]', (k) => k.classList.contains("armed"))), "ctrl stayed armed");
  await page.tap('.key[data-key="up"]');
  await page.keyboard.type("ls");
  await page.keyboard.press("Enter");
  await need("input");

  await need("diff");
  await live("after diff");

  // History fills the screen above a short grid; touch scrolling moves
  // through it, and reaching the top pages more in.
  const history = (text) => page.waitForFunction(
    (wanted) => (document.querySelector(".term-history") || {}).textContent?.includes(wanted),
    text, { timeout: 10000 });
  await history("row 499");
  const scrollTop = () => page.$eval(".term-viewport", (v) => v.scrollTop);
  const top0 = await scrollTop();
  if (which === "chromium") {
    const cdp = await context.newCDPSession(page);
    await cdp.send("Input.synthesizeScrollGesture", {
      x: 195, y: 300, yDistance: 600, gestureSourceType: "touch", speed: 2000,
    });
  } else {
    await page.mouse.move(195, 300);
    await page.mouse.wheel(0, -600);
  }
  await page.waitForTimeout(300);
  const top1 = await scrollTop();
  check(top1 < top0 - 100, "touch scroll did not move: " + top0 + " → " + top1);
  await page.evaluate(() => { document.querySelector(".term-viewport").scrollTop = 0; });
  await history("row 299");
  const before = await page.evaluate(() => {
    const v = document.querySelector(".term-viewport");
    v.scrollTop = 4000;
    return { top: v.scrollTop, rows: document.querySelectorAll(".term-history .term-row").length };
  });
  check(before.rows === 400, "expected two history pages, got " + before.rows);

  // A blip: the stream drops; the page reconnects and asks for only what
  // changed, keeping history and the reader's place.
  await need("drop");
  await live("after resume");
  const after = await page.evaluate(() => {
    const term = window.DiriTerminal.current;
    const v = document.querySelector(".term-viewport");
    return {
      top: v.scrollTop,
      rows: document.querySelectorAll(".term-history .term-row").length,
      text: document.querySelector(".term-live").textContent,
      stats: term && term.stats,
    };
  });
  check(after.stats, "terminal not exposed");
  check(after.stats.snapshots === 1, "reconnect sent a snapshot: " + JSON.stringify(after.stats));
  check(after.stats.resumes === 1, "no resume frame: " + JSON.stringify(after.stats));
  check(after.stats.resumeRows === 1, "resume resent unchanged rows: " + after.stats.resumeRows);
  check(after.text.includes("after diff"), "resume lost an unchanged row");
  check(after.rows === before.rows, "history lost on reconnect");
  check(Math.abs(after.top - before.top) < 2, "scroll position moved: " + before.top + " → " + after.top);

  await need("exit");
  await page.waitForFunction(() => !document.querySelector(".term-banner").hidden &&
    document.querySelector(".term-banner").textContent.includes("exit 0"), null, { timeout: 10000 });
  check(errors.length === 0, "page errors: " + errors.join("; "));
} catch (error) {
  failed = error;
} finally {
  await browser.close();
}
if (failed) {
  process.stdout.write("fail " + String(failed.message || failed).split("\n")[0] + "\n");
  process.exit(1);
}
process.stdout.write("pass\n");
process.exit(0);

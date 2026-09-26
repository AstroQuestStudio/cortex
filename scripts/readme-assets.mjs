// Generates the README terminal demo (docs/assets/demo-dark.svg, demo-light.svg) and the
// social preview source (.github/social-preview.svg). No dependencies: `node scripts/readme-assets.mjs`.
//
// The transcript below is the verbatim output of Cortex 0.3.0 on honojs/hono at commit
// 7c3b0df96dbf (the public benchmark checkout, `bench/public/run.sh hono`). Long lines are cut
// with "…"; the `read` listing skips lines 139-149 (marked "…"). Nothing else is edited.
//
// PNG of the social preview (1280x640), with any Chromium:
//   chrome --headless=new --disable-gpu --hide-scrollbars --window-size=1280,640 \
//          --screenshot=.github/social-preview.png .github/social-preview.svg

import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

const SCENES = [
  {
    cmd: 'cortex find "where is the request body parsed as form data"',
    out: [
      "S:src/utils/body.ts#parseFormData fn L126-150 — Parses form data from a request.",
      "S:src/request.ts#formData method L334-336 — Parses the request body as `FormData`.",
      "S:src/utils/body.ts#convertFormDataToBodyData fn L160-189 — Converts form data to body data based on…",
      "S:src/utils/body.ts#ParseBody interface L90-94 — Parses the body of a request based on the provided…",
      "S:src/middleware/body-limit/index.ts#bodyLimit fn L50-111 — Body Limit Middleware for Hono.",
      "…",
      "suite : card S:src/utils/body.ts#parseFormData",
    ],
  },
  {
    cmd: "cortex card S:src/utils/body.ts#parseFormData",
    out: [
      "S:src/utils/body.ts#parseFormData fn L126-150",
      "sig: async function parseFormData<T extends BodyData>( request: HonoRequest | Request, options: …",
      "rôle: Parses form data from a request.",
      "appelle 3 (+1 ambigu): S:src/utils/body.ts#isRawRequest, S:src/utils/body.ts#convertFormDataToBod…",
      "appelé par 1 (1 fichiers, L = ligne de l'appel): S:src/utils/body.ts#parseBody L112",
      "fichier importé par 4 fichier(s)",
      "tests: F:src/utils/body.test.ts",
      "autres exports du fichier 3: S:src/utils/body.ts#BodyData, S:src/utils/body.ts#ParseBodyOptions, …",
      "suite : read S:src/utils/body.ts#parseFormData",
    ],
  },
  {
    cmd: "cortex read S:src/utils/body.ts#parseFormData",
    out: [
      "S:src/utils/body.ts#parseFormData fn L126-150",
      "126│async function parseFormData<T extends BodyData>(",
      "127│  request: HonoRequest | Request,",
      "128│  options: ParseBodyOptions",
      "129│): Promise<T> {",
      "130│  if (!isRawRequest(request) && request.bodyCache.formData) {",
      "131│    return convertFormDataToBodyData<T>(",
      "132│      await (request.bodyCache.formData as FormData | Promise<FormData>),",
      "133│      options",
      "134│    )",
      "135│  }",
      "136│  const headers = isRawRequest(request) ? request.headers : request.raw.headers",
      "137│  const arrayBuffer = await (request as Request).arrayBuffer()",
      "138│  const formDataPromise = bufferToFormData(arrayBuffer, headers.get('Content-Type') || '')",
      "   …",
      "150│}",
      "suite : impact S:src/utils/body.ts#parseFormData",
    ],
  },
  {
    cmd: "cortex impact S:src/utils/body.ts#parseFormData",
    out: [
      "impact S:src/utils/body.ts#parseFormData fn L126-150",
      "10 dépendant(s) sur 3 niveau(x), 9 fichier(s)",
      "tests à relancer 4: F:src/middleware/method-override/index.test.ts, F:src/request.test.ts, …",
      "profondeur 1 — 1 (1 fichiers), L = ligne de l'appel",
      "  S:src/utils/body.ts#parseBody L112",
      "profondeur 2 — 3 (3 fichiers)",
      "  F:src/middleware/method-override/index.ts",
      "  F:src/request.ts",
      "  F:src/utils/body.test.ts",
      "profondeur 3 — 6 (6 fichiers)",
      "  F:src/context.ts",
      "  F:src/middleware/cache/index.ts",
      "  F:src/utils/body.ts",
      "  …",
      "suite : read S:src/utils/body.ts#parseBody",
    ],
  },
  {
    cmd: "# 4 calls, ~1,070 tokens: location, signature, callers, code, dependents, tests to rerun.",
    out: ["# grep + read to the same point: ~9,000 tokens, and still no dependents or tests."],
    comment: true,
  },
];

const THEMES = {
  dark: {
    page: "#0d1117", win: "#161b22", bar: "#1c2129", border: "#30363d",
    text: "#e6edf3", dim: "#8b949e", id: "#a5b4fc", prompt: "#3fb950", next: "#e3b341",
    dots: ["#f85149", "#d29922", "#3fb950"],
  },
  light: {
    page: "#ffffff", win: "#f6f8fa", bar: "#eaeef2", border: "#d0d7de",
    text: "#1f2328", dim: "#59636e", id: "#4f46e5", prompt: "#1a7f37", next: "#9a6700",
    dots: ["#cf222e", "#bf8700", "#1a7f37"],
  },
};

// Geometry: monospace cell of 8.4 x 21 px at 14 px. Every line gets textLength = chars x 8.4,
// so the typing mask lines up whatever monospace font the viewer has.
const FS = 14, CW = 8.4, LH = 21, PADX = 22, TOP = 58, W = 900;
const MAXLINES = Math.max(...SCENES.map((s) => s.out.length + 1));
const H = TOP + MAXLINES * LH + 18;
const MAXCHARS = Math.floor((W - 2 * PADX) / CW);
for (const s of SCENES) for (const l of [s.cmd, ...s.out]) {
  if (l.length + (l === s.cmd || s.comment ? 2 : 0) > MAXCHARS) throw new Error(`line too long (${l.length} > ${MAXCHARS}): ${l}`);
}

const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

// Split a line into [text, class] runs: identifiers, labels, line numbers, the "suite :" hint.
function runs(line, cmd = false) {
  if (line.startsWith("#")) {
    return line.split(/(~[\d,]+ tokens)/).filter(Boolean).map((t) => [t, /^~[\d,]+ tokens$/.test(t) ? "nx" : "dm"]);
  }
  if (cmd) return line.split(/([SFD]:[^\s,]+)/).filter(Boolean).map((t) => [t, /^[SFD]:/.test(t) ? "id" : ""]);
  if (line.startsWith("suite : ")) return [["suite : ", "dm"], [line.slice(8), "nx"]];
  const m = line.match(/^(\s*\d+)│(.*)$/);
  if (m) return [[m[1] + "│", "dm"], [m[2], ""]];
  const lab = line.match(/^([a-zà-ÿ][^:]*?:)(\s.*)$/);
  if (lab && !/[SFD]:/.test(line)) return [[lab[1], "dm"], [lab[2], ""]];
  const out = [];
  const re = /[SFD]:[^\s,]+/g;
  let last = 0, k;
  while ((k = re.exec(line))) {
    if (k.index > last) out.push([line.slice(last, k.index), last === 0 && /:\s*$/.test(line.slice(0, k.index)) ? "dm" : ""]);
    out.push([k[0], "id"]);
    last = k.index + k[0].length;
  }
  if (last < line.length) {
    const rest = line.slice(last);
    out.push([rest, / — /.test(rest) ? "" : "dm"]);
  }
  return out.length ? out : [[line, ""]];
}

function tl(chars) { return (chars * CW).toFixed(1); }

function textEl(x, y, str, cls, cmd = false) {
  if (!str.length) return "";
  const body = runs(str, cmd).map(([t, c]) => (c ? `<tspan class="${c}">${esc(t)}</tspan>` : esc(t))).join("");
  return `<text x="${x}" y="${y}" textLength="${tl(str.length)}" lengthAdjust="spacing" class="${cls}">${body}</text>`;
}

function build(theme) {
  const c = THEMES[theme];
  // Timeline (seconds).
  const TYPE = 0.034, LINE = 0.05;
  let t = 0.2;
  const plan = SCENES.map((s) => {
    const start = t;
    const typeStart = start + 0.45;
    const typeEnd = typeStart + Math.min(s.cmd.length * TYPE, 1.7);
    const outStart = typeEnd + 0.3;
    const hold = s.comment ? 3.8 : 2.9 + s.out.length * 0.05;
    const end = outStart + s.out.length * LINE + hold;
    t = end + 0.25;
    return { start, typeStart, typeEnd, outStart, end };
  });
  const T = t;
  const pc = (x) => ((x / T) * 100).toFixed(3) + "%";
  const eps = (x) => ((x / T) * 100 + 0.02).toFixed(3) + "%";

  const css = [];
  const body = [];
  let n = 0;
  const vis = (from, to) => {
    const name = `v${n++}`;
    css.push(`@keyframes ${name}{0%,${pc(from)}{opacity:0}${eps(from)},${pc(to)}{opacity:1}${eps(to)},100%{opacity:0}}`);
    css.push(`.${name}{animation:${name} ${T.toFixed(2)}s infinite}`);
    return name;
  };

  SCENES.forEach((s, i) => {
    const p = plan[i];
    const y0 = TOP;
    const g = [];
    const promptX = PADX, cmdX = PADX + 2 * CW;
    g.push(`<text x="${promptX}" y="${y0}" class="pr ${vis(p.start, p.end)}">$</text>`);
    const cmdCls = s.comment ? "dm" : "cm";
    const cmdVis = vis(p.start, p.end);
    g.push(textEl(cmdX, y0, s.cmd, `${cmdCls} ${cmdVis}`, true));
    // Typing mask + cursor, sliding right one cell per step.
    const w = s.cmd.length * CW;
    const name = `k${i}`;
    css.push(
      `@keyframes ${name}{0%{transform:translateX(0);opacity:0}${pc(p.start)}{opacity:0}${eps(p.start)}{opacity:1}` +
        `${pc(p.typeStart)}{transform:translateX(0);animation-timing-function:steps(${s.cmd.length},end)}` +
        `${pc(p.typeEnd)}{transform:translateX(${w.toFixed(1)}px);opacity:1}${eps(p.typeEnd)}{opacity:0}` +
        `100%{transform:translateX(${w.toFixed(1)}px);opacity:0}}`,
      `.${name}{animation:${name} ${T.toFixed(2)}s infinite}`,
    );
    g.push(
      `<g class="mask ${name}"><rect x="${cmdX}" y="${y0 - 15}" width="${(w + 3 * CW).toFixed(1)}" height="20" fill="${c.win}"/>` +
        `<rect x="${cmdX}" y="${y0 - 14}" width="${CW}" height="18" class="cur"/></g>`,
    );
    s.out.forEach((line, j) => {
      const y = y0 + (j + 1) * LH;
      const v = vis(p.outStart + j * LINE, p.end);
      if (s.comment) {
        g.push(`<text x="${promptX}" y="${y}" class="pr ${v}">$</text>`);
        g.push(textEl(cmdX, y, line, `dm ${v}`));
      } else {
        g.push(textEl(PADX, y, line, `tx ${v}`));
      }
    });
    body.push(`<g class="sc${i === 1 ? " still" : ""}">${g.join("")}</g>`);
  });

  const title = "hono · 421 files — 4 calls, ~1,070 tokens";
  const style = `
text{font-family:ui-monospace,SFMono-Regular,"SF Mono",Menlo,Consolas,"Liberation Mono","DejaVu Sans Mono",monospace;font-size:${FS}px;white-space:pre;fill:${c.text}}
.cm{font-weight:600}.dm{fill:${c.dim}}.id{fill:${c.id}}.nx{fill:${c.next};font-weight:600}.pr{fill:${c.prompt};font-weight:700}
.cur{fill:${c.text}}.ti{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;font-size:12.5px;fill:${c.dim}}
${css.join("\n")}
@media (prefers-reduced-motion:reduce){.sc *{animation:none!important}.sc:not(.still){display:none}.mask{display:none}}`;

  return `<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" role="img" aria-labelledby="t d">
<title id="t">Cortex terminal session on hono</title>
<desc id="d">Four chained calls: cortex find locates parseFormData, card shows its signature, callers and tests, read prints lines 126-150, impact lists 10 dependents and the 4 test files to rerun. About 1,070 tokens in total, versus about 9,000 with grep and file reads.</desc>
<style>${style}</style>
<rect x="0.5" y="0.5" width="${W - 1}" height="${H - 1}" rx="10" fill="${c.win}" stroke="${c.border}"/>
<path d="M0.5 10.5a10 10 0 0 1 10-10h${W - 21}a10 10 0 0 1 10 10v22h-${W - 1}z" fill="${c.bar}"/>
<line x1="0.5" y1="32.5" x2="${W - 0.5}" y2="32.5" stroke="${c.border}"/>
${c.dots.map((d, i) => `<circle cx="${20 + i * 18}" cy="16.5" r="5.5" fill="${d}"/>`).join("")}
<text x="${W / 2}" y="21" text-anchor="middle" class="ti">${esc(title)}</text>
${body.join("\n")}
</svg>
`;
}

function social() {
  const W = 1280, H = 640;
  const sans = `"Segoe UI Variable Display","Segoe UI",-apple-system,BlinkMacSystemFont,"Helvetica Neue",Arial,sans-serif`;
  const mono = `"Cascadia Mono",Consolas,ui-monospace,Menlo,monospace`;
  const lines = [
    ["$ ", "cortex find ", '"where is the request body parsed as form data"'],
    ["", "S:src/utils/body.ts#parseFormData", " fn L126-150"],
    ["", "suite : ", "card S:src/utils/body.ts#parseFormData"],
  ];
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}" viewBox="0 0 ${W} ${H}">
<defs>
  <radialGradient id="glow" cx="0.82" cy="0.05" r="0.9">
    <stop offset="0" stop-color="#4f46e5" stop-opacity="0.38"/>
    <stop offset="0.55" stop-color="#4f46e5" stop-opacity="0.06"/>
    <stop offset="1" stop-color="#4f46e5" stop-opacity="0"/>
  </radialGradient>
  <pattern id="grid" width="32" height="32" patternUnits="userSpaceOnUse">
    <path d="M32 0H0V32" fill="none" stroke="#ffffff" stroke-opacity="0.035"/>
  </pattern>
</defs>
<rect width="${W}" height="${H}" fill="#0b0d14"/>
<rect width="${W}" height="${H}" fill="url(#grid)"/>
<rect width="${W}" height="${H}" fill="url(#glow)"/>
<g font-family='${sans}'>
  <text x="96" y="222" font-size="132" font-weight="700" fill="#f5f7ff" letter-spacing="-4">Cortex</text>
  <text x="100" y="296" font-size="44" font-weight="600" fill="#e6e8f2">The code-context engine for AI agents</text>
  <text x="100" y="352" font-size="28" fill="#a0a6bd">Find, card, read, impact: understand a codebase in a few</text>
  <text x="100" y="390" font-size="28" fill="#a0a6bd">calls and ~10× fewer tokens than grep + read.</text>
  <text x="100" y="572" font-size="26" fill="#a0a6bd">by <tspan fill="#f5f7ff" font-weight="700">AstroQuest</tspan></text>
  <text x="1180" y="572" font-size="22" fill="#7d84a0" text-anchor="end">Open source (MIT) · Rust · MCP server · local</text>
</g>
<g transform="translate(100 432)">
  <rect width="1080" height="96" rx="12" fill="#12151f" stroke="#262b3d"/>
  <g font-family='${mono}' font-size="19">
    ${lines
      .map((l, i) => {
        const y = 30 + i * 25;
        const [a, b, d] = l;
        const first = a ? `<tspan fill="#3fb950" font-weight="700">${esc(a)}</tspan><tspan fill="#e6edf3" font-weight="600">${esc(b)}</tspan><tspan fill="#e6edf3">${esc(d)}</tspan>`
          : i === 1 ? `<tspan fill="#a5b4fc">${esc(b)}</tspan><tspan fill="#8b949e">${esc(d)}</tspan>`
          : `<tspan fill="#8b949e">${esc(b)}</tspan><tspan fill="#e3b341" font-weight="600">${esc(d)}</tspan>`;
        return `<text x="24" y="${y}">${first}</text>`;
      })
      .join("\n    ")}
  </g>
</g>
</svg>
`;
}

function write(rel, content) {
  const p = join(ROOT, rel);
  mkdirSync(dirname(p), { recursive: true });
  writeFileSync(p, content);
  console.log(`${rel}  ${content.length} bytes`);
}

write("docs/assets/demo-dark.svg", build("dark"));
write("docs/assets/demo-light.svg", build("light"));
write(".github/social-preview.svg", social());

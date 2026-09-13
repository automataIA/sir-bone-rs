import * as vscode from "vscode";
import * as cp from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

// Mirrors sirbone's project_slug (src/project_store.rs): every non-alphanumeric
// char of the absolute cwd becomes '-'.
function projectSlug(dir: string): string {
  return dir.replace(/[^a-zA-Z0-9]/g, "-");
}

function newSessionPath(cwd: string): string {
  const dir = path.join(os.homedir(), ".sirbone", "projects", projectSlug(cwd), "sessions");
  fs.mkdirSync(dir, { recursive: true });
  return path.join(dir, `${crypto.randomUUID()}.jsonl`);
}

export function activate(context: vscode.ExtensionContext) {
  const provider = new ChatViewProvider(context.extensionUri);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider("sirbone.chatView", provider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    // Reveal the sidebar view.
    vscode.commands.registerCommand("sirbone.openChat", () =>
      vscode.commands.executeCommand("sirbone.chatView.focus")
    ),
    // Header "+" button → start a fresh session.
    vscode.commands.registerCommand("sirbone.newChat", () => provider.reset()),
    // Header gear → this extension's settings.
    vscode.commands.registerCommand("sirbone.openSettings", () =>
      vscode.commands.executeCommand("workbench.action.openSettings", "@ext:sirbone.sirbone-vscode")
    ),
    // Credential onboarding.
    vscode.commands.registerCommand("sirbone.signIn", () => signIn().then(() => provider.refreshMeta()))
  );
  // Settings (model included) apply to the very next turn with no restart —
  // sessions re-read the config per spawn. This listener keeps the status bar
  // honest the moment a setting changes.
  vscode.workspace.onDidChangeConfiguration((e) => {
    if (e.affectsConfiguration("sirbone")) provider.refreshMeta();
  });
}

// The 5-hour quota window sirbone persists in ~/.sirbone/quota_window.json.
function readQuota(): { leftMs: number; pct: number } | undefined {
  try {
    const p = path.join(os.homedir(), ".sirbone", "quota_window.json");
    const w = JSON.parse(fs.readFileSync(p, "utf8"));
    const ms = (s: string) => Date.parse(String(s).replace(/(\.\d{3})\d+/, "$1"));
    const start = ms(w.start), end = ms(w.end), now = Date.now();
    if (!end || now >= end) return undefined; // no open window
    const total = end - start;
    // `used_pct` is the provider's own figure (GLM on z.ai); elsewhere fall back
    // to the share of the window elapsed.
    const elapsed = total > 0 ? Math.round((1 - (end - now) / total) * 100) : 0;
    return { leftMs: end - now, pct: typeof w.used_pct === "number" ? w.used_pct : elapsed };
  } catch {
    return undefined;
  }
}

const nonce = () =>
  Array.from({ length: 24 }, () => "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"[Math.floor(Math.random() * 62)]).join("");

function isExecutable(p: string): boolean {
  try {
    fs.accessSync(p, process.platform === "win32" ? fs.constants.F_OK : fs.constants.X_OK);
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}

// VS Code launched from the desktop often inherits a smaller PATH than an
// interactive shell. Resolve the configured command without invoking a shell,
// expand `~`, then try Cargo's standard user-level install location when the
// default command is used.
function resolveBinary(cwd: string): string {
  const configured = vscode.workspace.getConfiguration("sirbone").get<string>("binaryPath", "sirbone").trim() || "sirbone";
  const expanded = configured === "~"
    ? os.homedir()
    : configured.startsWith(`~${path.sep}`)
      ? path.join(os.homedir(), configured.slice(2))
      : configured;

  if (path.isAbsolute(expanded) || expanded.includes(path.sep)) return expanded;

  const executable = process.platform === "win32" && !expanded.endsWith(".exe") ? `${expanded}.exe` : expanded;
  for (const dir of (process.env.PATH ?? "").split(path.delimiter).filter(Boolean)) {
    const candidate = path.join(dir, executable);
    if (isExecutable(candidate)) return candidate;
  }

  if (configured === "sirbone") {
    const localName = process.platform === "win32" ? "sirbone.exe" : "sirbone";
    const cargoInstall = path.join(os.homedir(), ".cargo", "bin", localName);
    if (isExecutable(cargoInstall)) return cargoInstall;
  }

  return expanded;
}

// Onboarding: collect credentials in VS Code (key entered masked, never echoed),
// then hand them to `sirbone login` non-interactively — the key goes via stdin,
// so the core owns the secure 0600 write of ~/.sirbone/.env (one code path).
async function signIn() {
  const cwd = vscode.workspace.workspaceFolders?.[0].uri.fsPath ?? process.cwd();
  const bin = resolveBinary(cwd);
  const provider = await vscode.window.showQuickPick(
    [
      { label: "Anthropic / z.ai / GLM", description: "Claude-compatible (caching + thinking)", value: "anthropic" },
      { label: "OpenAI / Ollama / Groq / OpenRouter", description: "OpenAI-compatible", value: "openai" },
    ],
    { placeHolder: "Select your provider", ignoreFocusOut: true }
  );
  if (!provider) return;
  const anthropic = provider.value === "anthropic";
  const baseUrl = await vscode.window.showInputBox({
    prompt: "Base URL (leave blank for the provider's official endpoint)",
    value: anthropic ? "https://api.z.ai/api/anthropic" : "",
    ignoreFocusOut: true,
  });
  if (baseUrl === undefined) return;
  const model = await vscode.window.showInputBox({
    prompt: "Model id",
    value: anthropic ? "glm-5.3" : "gpt-4o-mini",
    ignoreFocusOut: true,
  });
  if (model === undefined) return;
  const key = await vscode.window.showInputBox({
    prompt: "API key / token (stored in ~/.sirbone/.env, chmod 600)",
    password: true,
    ignoreFocusOut: true,
  });
  if (!key) return;

  const args = ["login", "--login-provider", provider.value];
  if (baseUrl.trim()) args.push("--login-base-url", baseUrl.trim());
  if (model.trim()) args.push("--login-model", model.trim());
  await new Promise<void>((resolve) => {
    const child = cp.spawn(bin, args, { cwd, env: process.env });
    let out = "";
    child.stdout.on("data", (d) => (out += d.toString()));
    child.stderr.on("data", (d) => (out += d.toString()));
    child.on("error", (e) => { vscode.window.showErrorMessage(`Sir Bone sign-in failed: ${e.message}`); resolve(); });
    child.on("close", (code) => {
      if (code === 0) vscode.window.showInformationMessage("Sir Bone: credentials saved to ~/.sirbone/.env — you can chat now.");
      else vscode.window.showErrorMessage(`Sir Bone sign-in failed: ${out.trim() || "exit " + code}`);
      resolve();
    });
    child.stdin.write(key);
    child.stdin.end();
  });
}

// Provider + model from `sirbone doctor` (no API call, ~1s). `env` must be the
// same effective environment the chat sessions get (buildEnv): the VS Code
// process itself may carry a stale exported SIRBONE_MODEL that dotenvy would
// never override, which made the status bar lie after a settings change.
function readDoctor(bin: string, cwd: string, env: NodeJS.ProcessEnv, cb: (m: { provider?: string; model?: string }) => void) {
  let out = "";
  const child = cp.spawn(bin, ["doctor"], { cwd, env });
  child.stdout.on("data", (d) => (out += d.toString()));
  child.on("error", () => cb({}));
  child.on("close", () => {
    const provider = out.match(/provider:\s*(\S+)/)?.[1];
    const model = out.match(/\bmodel:\s*(\S+)/)?.[1];
    cb({ provider, model });
  });
}

// ── Session history (per project) ──────────────────────────────────────────
function sessionsDir(cwd: string): string {
  return path.join(os.homedir(), ".sirbone", "projects", projectSlug(cwd), "sessions");
}

function sessionTitle(p: string): string {
  try {
    for (const l of fs.readFileSync(p, "utf8").split("\n")) {
      if (!l) continue;
      let r: any;
      try { r = JSON.parse(l); } catch { continue; }
      if (r.type === "message" && r.role === "user" && Array.isArray(r.content)) {
        const t = r.content.find((b: any) => b.type === "text");
        if (t?.text) return String(t.text).replace(/\s+/g, " ").trim().slice(0, 60);
      }
    }
  } catch { /* ignore */ }
  return "Untitled";
}

function listSessions(cwd: string): { id: string; title: string; ts: number }[] {
  let files: string[] = [];
  try { files = fs.readdirSync(sessionsDir(cwd)).filter((f) => f.endsWith(".jsonl")); } catch { return []; }
  const out: { id: string; title: string; ts: number }[] = [];
  for (const f of files) {
    const p = path.join(sessionsDir(cwd), f);
    try {
      const st = fs.statSync(p);
      if (st.size === 0) continue;
      out.push({ id: f, title: sessionTitle(p), ts: st.mtimeMs });
    } catch { /* skip */ }
  }
  return out.sort((a, b) => b.ts - a.ts);
}

// Installed skills (global + project), mirroring sirbone's scan for the / menu.
function parseSkillMeta(mdPath: string): { name: string; desc: string } | null {
  try {
    let name = "", desc = "", inFm = false;
    for (const l of fs.readFileSync(mdPath, "utf8").split("\n").slice(0, 40)) {
      if (l.trim() === "---") { if (!inFm) { inFm = true; continue; } else break; }
      const nm = l.match(/^name:\s*(.+)$/); if (nm) name = nm[1].trim().replace(/^["']|["']$/g, "");
      const dm = l.match(/^description:\s*(.+)$/); if (dm) desc = dm[1].trim().replace(/^["']|["']$/g, "");
    }
    return name ? { name, desc } : null;
  } catch { return null; }
}

function scanSkillDir(dir: string, out: Map<string, { name: string; desc: string }>) {
  let entries: fs.Dirent[] = [];
  try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
  for (const e of entries) {
    if (!e.isDirectory()) continue;
    for (const f of ["SKILL.md", "skill.md"]) {
      const p = path.join(dir, e.name, f);
      if (fs.existsSync(p)) { const m = parseSkillMeta(p); if (m) out.set(m.name, m); break; }
    }
  }
}

function enabledSkills(cwd: string): Set<string> {
  try {
    const cfg = JSON.parse(fs.readFileSync(path.join(os.homedir(), ".sirbone", "projects", projectSlug(cwd), "config.json"), "utf8"));
    return new Set<string>(cfg?.skills?.enabled ?? []);
  } catch { return new Set(); }
}

function listSkills(cwd: string): { name: string; desc: string; enabled: boolean }[] {
  const out = new Map<string, { name: string; desc: string }>();
  scanSkillDir(path.join(os.homedir(), ".sirbone", "skills"), out); // global
  scanSkillDir(path.join(cwd, ".sirbone", "skills"), out);          // project overrides
  const en = enabledSkills(cwd);
  return [...out.values()].map((s) => ({ ...s, enabled: en.has(s.name) })).sort((a, b) => a.name.localeCompare(b.name));
}

// ── Project config: enable/disable skills & MCP servers (settings panel) ─────
function projectConfigPath(cwd: string): string {
  return path.join(os.homedir(), ".sirbone", "projects", projectSlug(cwd), "config.json");
}
function readProjectConfig(cwd: string): any {
  try { return JSON.parse(fs.readFileSync(projectConfigPath(cwd), "utf8")); } catch { return {}; }
}
function writeProjectArray(cwd: string, section: string, names: string[]) {
  const cfg = readProjectConfig(cwd);
  cfg[section] = { ...(cfg[section] || {}), enabled: names };
  fs.mkdirSync(path.dirname(projectConfigPath(cwd)), { recursive: true });
  fs.writeFileSync(projectConfigPath(cwd), JSON.stringify(cfg, null, 2));
}
function toggleEnabled(cwd: string, section: "skills" | "mcp", name: string, on: boolean) {
  const cur = new Set<string>(readProjectConfig(cwd)?.[section]?.enabled ?? []);
  if (on) cur.add(name); else cur.delete(name);
  writeProjectArray(cwd, section, [...cur]);
}
function listMcp(cwd: string): { name: string; cmd: string; enabled: boolean }[] {
  const defs = new Map<string, { command: string; args: string[] }>();
  const read = (f: string) => {
    try {
      const j = JSON.parse(fs.readFileSync(f, "utf8"));
      for (const [k, v] of Object.entries<any>(j?.mcpServers ?? {})) defs.set(k, { command: v.command ?? "", args: v.args ?? [] });
    } catch { /* ignore */ }
  };
  read(path.join(os.homedir(), ".sirbone", "mcp.json"));
  read(path.join(cwd, ".mcp.json"));
  const en = new Set<string>(readProjectConfig(cwd)?.mcp?.enabled ?? []);
  return [...defs.entries()]
    .map(([name, v]) => ({ name, cmd: [v.command, ...v.args].filter(Boolean).join(" "), enabled: en.has(name) }))
    .sort((a, b) => a.name.localeCompare(b.name));
}

// Project files tracked by git (naturally respects .gitignore) for @-mentions.
function listFiles(cwd: string): string[] {
  try {
    return cp
      .execFileSync("git", ["ls-files"], { cwd, maxBuffer: 16 * 1024 * 1024 })
      .toString()
      .split("\n")
      .filter(Boolean)
      .slice(0, 8000);
  } catch {
    return [];
  }
}

// Ordered render events for replaying a saved session (includes user messages).
function sessionEvents(p: string): any[] {
  const evts: any[] = [];
  let lines: string[] = [];
  try { lines = fs.readFileSync(p, "utf8").split("\n"); } catch { return evts; }
  for (const l of lines) {
    if (!l) continue;
    let r: any;
    try { r = JSON.parse(l); } catch { continue; }
    if (r.type !== "message" || !Array.isArray(r.content)) continue;
    for (const b of r.content) {
      if (b.type === "text") evts.push({ type: r.role === "user" ? "user" : "assistant", text: b.text });
      else if (b.type === "thinking") evts.push({ type: "thinking", text: b.thinking ?? b.text ?? "" });
      else if (b.type === "tool_use") evts.push({ type: "tool", id: b.id, name: b.name, input: b.input, ts: r.ts });
      else if (b.type === "tool_result")
        evts.push({ type: "toolresult", id: b.tool_use_id, isError: !!b.is_error, content: typeof b.content === "string" ? b.content : JSON.stringify(b.content), ts: r.ts });
    }
  }
  return evts;
}

// Map the extension settings that mirror sirbone's TUI toggles onto the child env.
function buildEnv(cfg: vscode.WorkspaceConfiguration, ctxWindow: number): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env, SIRBONE_CONTEXT_WINDOW: String(ctxWindow) };
  const model = cfg.get<string>("model", "").trim();
  // Set → wins over everything (including a stale exported SIRBONE_MODEL in the
  // VS Code process env). Unset → strip the inherited var so sirbone falls back
  // to its .env files instead of being pinned to whatever VS Code was launched with.
  if (model) env.SIRBONE_MODEL = model;
  else delete env.SIRBONE_MODEL;
  const tb = cfg.get<number>("thinkingBudget", 0);
  if (tb > 0) env.SIRBONE_THINKING_BUDGET = String(tb);
  const ms = cfg.get<number>("maxSteps", 0);
  if (ms > 0) env.SIRBONE_MAX_STEPS = String(ms);
  if (cfg.get<boolean>("planMode", false)) env.SIRBONE_PLAN = "1";
  if (!cfg.get<boolean>("grounding", true)) env.SIRBONE_NO_GROUNDING = "1";
  if (!cfg.get<boolean>("localize", true)) env.SIRBONE_NO_LOCALIZE = "1";
  if (!cfg.get<boolean>("groundContext", true)) env.SIRBONE_NO_GROUND_CONTEXT = "1";
  if (!cfg.get<boolean>("compaction", true)) env.SIRBONE_NO_COMPACT = "1";
  if (!cfg.get<boolean>("snapshots", true)) env.SIRBONE_NO_SNAPSHOT = "1";
  if (!cfg.get<boolean>("historia", true)) env.SIRBONE_NO_HISTORIA = "1";
  if (cfg.get<boolean>("ace", false)) env.SIRBONE_ACE = "1";
  if (cfg.get<boolean>("hygiene", false)) env.SIRBONE_HYGIENE = "1";
  if (cfg.get<boolean>("yagni", false)) env.SIRBONE_YAGNI = "1";
  return env;
}

// Chat lives as a webview view in the Sir Bone container.
class ChatViewProvider implements vscode.WebviewViewProvider {
  private view?: vscode.WebviewView;
  private sessionPath: string | undefined;
  private busy = false;
  private activeChild: cp.ChildProcess | null = null;
  // Temp files for images pasted into the composer, flushed into the next turn.
  private pendingImages: string[] = [];

  constructor(private readonly extensionUri: vscode.Uri) {}

  reset() {
    this.sessionPath = undefined;
    this.view?.webview.postMessage({ type: "cleared" });
  }

  // Push live provider/model (from `sirbone doctor`) + quota to the status bar.
  refreshMeta() {
    const v = this.view;
    if (!v) return;
    const cwd = vscode.workspace.workspaceFolders?.[0].uri.fsPath ?? process.cwd();
    const bin = resolveBinary(cwd);
    v.webview.postMessage({ type: "status", quota: readQuota() });
    readDoctor(bin, cwd, buildEnv(vscode.workspace.getConfiguration("sirbone"), vscode.workspace.getConfiguration("sirbone").get<number>("contextWindow", 200000)), (m) => v.webview.postMessage({ type: "meta", ...m }));
  }

  private uri(view: vscode.WebviewView, ...p: string[]) {
    return view.webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, ...p)).toString();
  }

  resolveWebviewView(view: vscode.WebviewView) {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [this.extensionUri],
    };
    const assets = {
      icon: this.uri(view, "images", "icon.svg"),
      marked: this.uri(view, "media", "vendor", "marked.umd.js"),
      purify: this.uri(view, "media", "vendor", "purify.min.js"),
      hljs: this.uri(view, "media", "vendor", "highlight.min.js"),
      mermaid: this.uri(view, "media", "vendor", "mermaid.min.js"),
      hljsDark: this.uri(view, "media", "vendor", "hljs-dark.css"),
      hljsLight: this.uri(view, "media", "vendor", "hljs-light.css"),
    };
    view.webview.html = getHtml(view.webview, assets, nonce());
    const cwd = vscode.workspace.workspaceFolders?.[0].uri.fsPath ?? process.cwd();
    const refreshMeta = () => this.refreshMeta();

    view.webview.onDidReceiveMessage((msg) => {
      if (msg?.type === "ready") { refreshMeta(); return; }
      if (msg?.type === "settings")
        return void vscode.commands.executeCommand("workbench.action.openSettings", "@ext:sirbone.sirbone-vscode");
      if (msg?.type === "signIn") { signIn().then(refreshMeta); return; }
      if (msg?.type === "history")
        return void view.webview.postMessage({ type: "sessions", list: listSessions(cwd) });
      if (msg?.type === "files")
        return void view.webview.postMessage({ type: "files", list: listFiles(cwd) });
      if (msg?.type === "skills")
        return void view.webview.postMessage({ type: "skills", list: listSkills(cwd) });
      if (msg?.type === "config") {
        refreshMeta();
        return void view.webview.postMessage({ type: "config", skills: listSkills(cwd), mcp: listMcp(cwd) });
      }
      if (msg?.type === "toggleSkill" || msg?.type === "toggleMcp") {
        toggleEnabled(cwd, msg.type === "toggleSkill" ? "skills" : "mcp", String(msg.name), !!msg.on);
        return void view.webview.postMessage({ type: "config", skills: listSkills(cwd), mcp: listMcp(cwd) });
      }
      if (msg?.type === "open") {
        const p = path.join(sessionsDir(cwd), path.basename(String(msg.id)));
        this.sessionPath = p;
        return void view.webview.postMessage({ type: "load", title: sessionTitle(p), events: sessionEvents(p) });
      }
      if (msg?.type === "reset") return this.reset();
      // Route a prompt answer back to the running child's stdin control channel.
      if (msg?.type === "promptReply") {
        const line = JSON.stringify({ type: "reply", id: msg.id, index: msg.index, text: msg.text }) + "\n";
        try { this.activeChild?.stdin?.write(line); } catch { /* child gone */ }
        return;
      }
      // Pasted image: spill it to a temp file and hand sirbone the path. `--image`
      // re-homes it into the session's attachment store, so the extension does
      // not duplicate that bookkeeping.
      if (msg?.type === "attach") {
        try {
          const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sirbone-paste-"));
          const file = path.join(dir, String(msg.name ?? "image.png"));
          fs.writeFileSync(file, Buffer.from(String(msg.data ?? ""), "base64"));
          this.pendingImages.push(file);
          view.webview.postMessage({ type: "attached", label: `image_${this.pendingImages.length}` });
        } catch (e: any) {
          view.webview.postMessage({ type: "error", text: `attach failed: ${e.message}` });
        }
        return;
      }
      if (msg?.type !== "ask" || this.busy) return;
      const text = String(msg.text ?? "").trim();
      if (!text) return;
      this.busy = true;
      if (!this.sessionPath) this.sessionPath = newSessionPath(cwd);
      const images = this.pendingImages.splice(0);
      this.activeChild = runTurn(view.webview, cwd, this.sessionPath, text, images, () => {
        this.busy = false;
        this.activeChild = null;
      });
    });
  }
}

interface Assets {
  icon: string; marked: string; purify: string; hljs: string;
  mermaid: string; hljsDark: string; hljsLight: string;
}

// Drive one turn via `--output-format stream-json`: sirbone emits one NDJSON event
// per line on stdout as it happens, so the webview renders live (typewriter text,
// tools as they start/finish). sirbone still writes the session file for history.
function runTurn(
  webview: vscode.Webview,
  cwd: string,
  sessionPath: string,
  prompt: string,
  images: string[],
  done: () => void
) {
  const cfg = vscode.workspace.getConfiguration("sirbone");
  const bin = resolveBinary(cwd);
  const extra = cfg.get<string[]>("extraArgs", []);
  const ctxWindow = cfg.get<number>("contextWindow", 200000);
  const env = buildEnv(cfg, ctxWindow);
  const pct = (used: number) => (ctxWindow > 0 && used ? Math.min(100, Math.round((used / ctxWindow) * 100)) : null);

  // `--input-format stream-json` opens the control channel: sirbone emits
  // {type:"ask",id,prompt} when it needs a decision and reads our
  // {type:"reply",id,index?,text?} from stdin. Lets the user answer permission
  // gates and `ask_user` questions from the webview instead of auto-denying.
  const imageArgs = images.flatMap((p) => ["--image", p]);
  const child = cp.spawn(bin, ["-p", "--output-format", "stream-json", "--input-format", "stream-json", "--session", sessionPath, ...imageArgs, ...extra, prompt], { cwd, env });

  let sawResult = false;
  let buf = "";
  let err = "";
  const handle = (line: string) => {
    const t = line.trim();
    if (!t) return;
    let e: any;
    try { e = JSON.parse(t); } catch { return; }
    switch (e.type) {
      case "text": webview.postMessage({ type: "stream_text", text: e.text }); break;
      case "thinking": webview.postMessage({ type: "stream_thinking", text: e.text }); break;
      case "tool_start": webview.postMessage({ type: "tool", id: e.id, name: e.name, input: e.input, ts: new Date().toISOString() }); break;
      case "tool_end": webview.postMessage({ type: "toolresult", id: e.id, isError: !!e.is_error, content: e.content, ts: new Date().toISOString() }); break;
      case "ctx": { const p = pct(Number(e.used)); if (p != null) webview.postMessage({ type: "stream_ctx", ctxPct: p }); break; }
      case "ask": webview.postMessage({ type: "prompt", id: e.id, prompt: e.prompt }); break;
      case "error": webview.postMessage({ type: "error", text: e.text }); break;
      case "result": {
        sawResult = true;
        webview.postMessage({ type: "done", usage: e.usage, ctxPct: pct(Number(e.usage?.peak_context ?? 0)), quota: readQuota() });
        break;
      }
    }
  };

  child.stdout.on("data", (d) => {
    buf += d.toString();
    let nl: number;
    while ((nl = buf.indexOf("\n")) >= 0) {
      handle(buf.slice(0, nl));
      buf = buf.slice(nl + 1);
    }
  });
  child.stderr.on("data", (d) => (err += d.toString()));

  let spawnFailed = false;
  child.on("error", (e: NodeJS.ErrnoException) => {
    spawnFailed = true;
    const text = e.code === "ENOENT"
      ? `Sir Bone executable not found: ${bin}. Set sirbone.binaryPath to the installed binary.`
      : `Unable to start Sir Bone (${bin}): ${e.message}`;
    webview.postMessage({ type: "error", text });
    done();
  });
  child.on("close", (code) => {
    if (spawnFailed) return;
    if (buf.trim()) handle(buf);
    if (!sawResult) {
      const text = err.trim() || `sirbone exited with code ${code}`;
      webview.postMessage({ type: "error", text });
      if (/no API key/i.test(text)) {
        vscode.window.showErrorMessage("Sir Bone: no API key configured.", "Sign in").then((s) => { if (s === "Sign in") signIn(); });
      }
    }
    done();
  });
  return child;
}

function getHtml(webview: vscode.Webview, a: Assets, cspNonce: string): string {
  // Claude Code / Codex-style chat: markdown (marked) + mermaid + highlight.js
  // rendered from local bundles under a nonce CSP; Lucide icons in the status bar;
  // TUI-like collapsible tool boxes. JS is scoped to the postMessage bridge and the
  // render helpers; layout/animation/state stays in CSS (@keyframes, :has()).
  return /* html */ `<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8" />
<meta http-equiv="Content-Security-Policy"
  content="default-src 'none'; img-src ${webview.cspSource} data:; font-src ${webview.cspSource}; style-src ${webview.cspSource} 'unsafe-inline'; script-src 'nonce-${cspNonce}' 'unsafe-eval';" />
<link id="hljs-dark" rel="stylesheet" href="${a.hljsDark}" />
<link id="hljs-light" rel="stylesheet" href="${a.hljsLight}" disabled />
<style>
  :root {
    --fg: var(--vscode-foreground);
    --muted: var(--vscode-descriptionForeground);
    --border: var(--vscode-widget-border, var(--vscode-panel-border));
    --accent: var(--vscode-textLink-foreground);
    --field: var(--vscode-input-background);
    --code-bg: var(--vscode-textCodeBlock-background, rgba(127,127,127,.09));
    --ok: var(--vscode-charts-green, #3fb950);
    --warn: var(--vscode-charts-yellow, #d29922);
    --danger: var(--vscode-errorForeground);
    --radius: 14px;
  }
  * { box-sizing: border-box; }
  body { font-family: var(--vscode-font-family); font-size: 13px; color: var(--fg);
         margin: 0; height: 100vh; display: flex; flex-direction: column; position: relative; }
  #log { flex: 1; min-height: 0; overflow-y: auto; padding: 16px 14px;
         display: flex; flex-direction: column; gap: 12px; overscroll-behavior: contain; }
  #end { overflow-anchor: auto; height: 1px; }

  /* Top bar (title + history/settings/new), like Codex/Claude Code */
  #topbar { display: flex; align-items: center; gap: 8px; padding: 8px 10px;
            border-bottom: 1px solid var(--border); }
  #title { font-weight: 600; font-size: 13px; white-space: nowrap; overflow: hidden;
           text-overflow: ellipsis; flex: 1; }
  .tb-actions { display: flex; gap: 2px; flex: none; }
  .tb-btn { background: none; border: none; color: var(--muted); cursor: pointer;
            padding: 4px; border-radius: 5px; display: grid; place-items: center; }
  .tb-btn:hover { background: var(--code-bg); color: var(--fg); }
  .tb-btn .ic { width: 15px; height: 15px; }

  /* History overlay */
  #history { position: absolute; inset: 0; background: var(--vscode-editor-background, var(--field));
             display: flex; flex-direction: column; z-index: 5; }
  #history[hidden] { display: none; }
  .hist-head { display: flex; align-items: center; justify-content: space-between;
               padding: 10px 12px; border-bottom: 1px solid var(--border); font-weight: 600; font-size: 12px; color: var(--muted); }
  #hist-list { flex: 1; overflow-y: auto; padding: 6px; }
  .hist-item { display: flex; align-items: baseline; gap: 8px; padding: 9px 10px; border-radius: 8px; cursor: pointer; }
  .hist-item:hover { background: var(--code-bg); }
  .hist-title { flex: 1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .hist-time { flex: none; color: var(--muted); font-size: 11px; }
  .hist-empty { color: var(--muted); text-align: center; padding: 24px; font-size: 12px; }

  /* Settings panel */
  #settings-panel { position: absolute; inset: 0; background: var(--vscode-editor-background, var(--field));
                    display: flex; flex-direction: column; z-index: 5; }
  #settings-panel[hidden] { display: none; }
  #set-body { flex: 1; overflow-y: auto; padding: 6px 12px 16px; }
  .set-sec { padding: 12px 0; border-bottom: 1px solid var(--border); }
  .set-sec:last-child { border-bottom: none; }
  .set-sec-h { font-weight: 600; font-size: 12px; margin-bottom: 4px; }
  .set-hint { font-weight: 400; font-size: 10px; color: var(--muted); margin-left: 6px; }
  .set-note { font-size: 11px; color: var(--muted); margin-bottom: 8px; line-height: 1.4; }
  .set-link { background: none; border: 1px solid var(--border); border-radius: 6px; color: var(--accent);
              cursor: pointer; font: inherit; font-size: 12px; padding: 5px 10px; }
  .set-link:hover { background: var(--code-bg); }
  .set-list { display: flex; flex-direction: column; gap: 2px; }
  .set-row { display: flex; align-items: flex-start; gap: 10px; padding: 8px; border-radius: 8px; cursor: pointer; }
  .set-row:hover { background: var(--code-bg); }
  .set-box { flex: none; width: 16px; height: 16px; margin-top: 1px; border-radius: 4px;
             border: 1.5px solid color-mix(in srgb, var(--fg) 45%, transparent);
             background: var(--vscode-checkbox-background, var(--field));
             display: grid; place-items: center; font-size: 12px; line-height: 1; }
  .set-row:hover .set-box { border-color: var(--fg); }
  .set-row.on .set-box { background: var(--accent); border-color: var(--accent); color: var(--vscode-button-foreground, #fff); }
  .set-main { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
  .set-name { font-weight: 600; font-family: var(--vscode-editor-font-family); font-size: 12px; }
  .set-desc { color: var(--muted); font-size: 11px; line-height: 1.45; white-space: normal; word-break: break-word; }
  .set-empty { color: var(--muted); font-size: 11px; padding: 6px 8px; }

  #empty { margin: auto; text-align: center; max-width: 320px; overflow-anchor: none;
           display: flex; flex-direction: column; align-items: center; gap: 14px; color: var(--muted); }
  #log:has(.row) #empty { display: none; }
  #empty img { width: 60px; height: 60px; opacity: .9; animation: float 4s ease-in-out infinite; }
  #empty h1 { margin: 0; font-size: 15px; font-weight: 600; color: var(--fg); }
  #empty p { margin: 0; line-height: 1.5; }
  @keyframes float { 0%,100% { transform: translateY(0); } 50% { transform: translateY(-5px); } }
  .chips { display: flex; flex-wrap: wrap; gap: 8px; justify-content: center; margin-top: 2px; }
  .chip { border: 1px solid var(--border); border-radius: 999px; padding: 5px 12px; cursor: pointer;
          color: var(--fg); background: transparent; font: inherit; font-size: 12px;
          transition: background .15s, border-color .15s; }
  .chip:hover { background: var(--code-bg); border-color: var(--accent); }

  .row { overflow-anchor: none; animation: pop .18s ease-out; }
  @keyframes pop { from { opacity: 0; transform: translateY(6px); } to { opacity: 1; transform: none; } }
  @media (prefers-reduced-motion: reduce) { .row, #empty img { animation: none; } }
  .user { align-self: flex-end; max-width: 88%; background: var(--field); border: 1px solid var(--border);
          border-radius: var(--radius) var(--radius) 4px var(--radius); padding: 9px 12px;
          white-space: pre-wrap; word-break: break-word; }
  .thinking { align-self: stretch; color: var(--muted); font-style: italic; font-size: 12px;
              border-left: 2px solid var(--border); padding: 1px 0 1px 10px; white-space: pre-wrap; word-break: break-word; }
  /* Streaming (typewriter): raw text with a blinking caret until the block finalizes. */
  .row.streaming { white-space: pre-wrap; word-break: break-word; }
  .row.streaming::after { content: '▋'; margin-left: 1px; color: var(--muted); animation: blink 1s steps(2, start) infinite; }
  @keyframes blink { 50% { opacity: 0; } }
  @media (prefers-reduced-motion: reduce) { .row.streaming::after { animation: none; } }
  .err { align-self: stretch; color: var(--danger); font-size: 12px; }
  .err::before { content: '⚠ '; }

  /* Markdown (assistant) */
  .assistant { align-self: stretch; line-height: 1.6; word-break: break-word; }
  .assistant > :first-child { margin-top: 0; }
  .assistant > :last-child { margin-bottom: 0; }
  .assistant p { margin: 6px 0; }
  .assistant h1,.assistant h2,.assistant h3,.assistant h4 { margin: 12px 0 6px; line-height: 1.3; }
  .assistant h1 { font-size: 1.3em; } .assistant h2 { font-size: 1.15em; } .assistant h3 { font-size: 1.05em; }
  .assistant ul,.assistant ol { margin: 6px 0; padding-left: 20px; }
  .assistant li { margin: 2px 0; }
  .assistant code { font-family: var(--vscode-editor-font-family); font-size: .92em; background: var(--code-bg); padding: 1px 5px; border-radius: 4px; }
  .assistant pre { background: var(--code-bg); border: 1px solid var(--border); border-radius: 8px; padding: 10px 12px; overflow-x: auto; margin: 8px 0; }
  .assistant pre code { background: none; padding: 0; font-size: .9em; }
  .assistant table { border-collapse: collapse; margin: 8px 0; display: block; overflow-x: auto; }
  .assistant th,.assistant td { border: 1px solid var(--border); padding: 4px 8px; text-align: left; }
  .assistant th { background: var(--code-bg); }
  .assistant a { color: var(--accent); }
  .assistant blockquote { border-left: 3px solid var(--border); margin: 6px 0; padding: 2px 10px; color: var(--muted); }
  .mermaid { margin: 8px 0; text-align: center; }
  .mmfail { border: 1px solid var(--warn); border-radius: 8px; padding: 8px 10px; background: var(--code-bg); overflow-x: auto; margin: 8px 0; }
  .mmfail::before { content: '⚠ diagram error — source below'; display: block; color: var(--warn); font-size: 11px; margin-bottom: 6px; }
  .mmfail code { font-family: var(--vscode-editor-font-family); font-size: .9em; white-space: pre; }

  /* Tool box (TUI / Claude Code style): bold larger name + labeled IN/OUT rows */
  .tool { align-self: stretch; margin: 2px 0; }
  .t-head { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
  .t-head .ic { color: var(--accent); width: 15px; height: 15px; }
  .t-name { font-weight: 700; font-size: 13.5px; color: var(--fg); font-family: var(--vscode-editor-font-family); }
  .t-meta { color: var(--muted); font-size: 11px; }
  .t-io { border: 1px solid var(--border); border-radius: 10px; overflow: hidden;
          font-family: var(--vscode-editor-font-family); font-size: 12px; background: var(--code-bg); }
  .t-line { display: flex; gap: 10px; padding: 8px 11px; align-items: baseline; }
  .t-inline { background: color-mix(in srgb, var(--fg) 4%, transparent); }
  .t-outline { border-top: 1px solid var(--border); }
  .t-io:has(.t-out:empty) .t-outline { display: none; }
  .t-lbl { flex: none; width: 30px; text-align: center; color: var(--muted); font-size: 9px; font-weight: 700;
           letter-spacing: .5px; border: 1px solid var(--border); border-radius: 5px; padding: 1px 0; background: var(--field); }
  .t-in { color: var(--fg); white-space: pre-wrap; word-break: break-all; flex: 1; min-width: 0; }
  .t-out { color: var(--muted); flex: 1; min-width: 0; }
  .tool.err .t-io { border-color: var(--danger); }
  .tool.err .t-outline .t-lbl { color: var(--danger); border-color: var(--danger); }
  .tool.err .t-out { color: var(--danger); }
  .t-outbody { white-space: pre-wrap; word-break: break-word; max-height: 240px; overflow: auto; }
  .t-expand { display: block; margin-top: 4px; background: none; border: none; color: var(--accent);
              cursor: pointer; font: inherit; font-size: 11px; padding: 0; }

  /* Todo checklist (todo tool): live plan view, Claude Code style */
  .todo-list { display: flex; flex-direction: column; gap: 3px; }
  .todo-item { color: var(--fg); }
  .todo-item .tmark { display: inline-block; width: 1.3em; color: var(--muted); }
  .todo-item.completed { color: var(--muted); text-decoration: line-through; }
  .todo-item.completed .tmark { color: var(--ok, #3fb950); text-decoration: none; }
  .todo-item.in_progress { font-weight: 700; }
  .todo-item.in_progress .tmark { color: var(--accent); }

  /* Composer */
  #busy { display: none; }
  #bar { padding: 10px 12px 12px; border-top: 1px solid var(--border); }
  #composer-wrap { position: relative; }
  #suggest { position: absolute; bottom: 100%; left: 0; right: 0; margin-bottom: 6px;
             background: var(--vscode-editor-background, var(--field)); border: 1px solid var(--border);
             border-radius: 10px; max-height: 240px; overflow-y: auto; box-shadow: 0 6px 20px rgba(0,0,0,.35); z-index: 6; }
  #suggest[hidden] { display: none; }
  .sg-item { padding: 7px 12px; cursor: pointer; display: flex; gap: 8px; align-items: baseline; }
  .sg-item.sel { background: var(--code-bg); }
  .sg-cmd { font-weight: 600; color: var(--fg); flex: none; }
  .sg-tag { flex: none; font-size: 9px; font-weight: 700; letter-spacing: .3px; color: var(--accent);
            border: 1px solid var(--accent); border-radius: 5px; padding: 0 4px; opacity: .9; }
  .sg-tag.off { color: var(--muted); border-color: var(--border); }
  .sg-desc { color: var(--muted); font-size: 11px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .sg-path { font-family: var(--vscode-editor-font-family); font-size: 12px; color: var(--fg);
             overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  #composer { display: flex; align-items: flex-end; gap: 8px; background: var(--field);
              border: 1px solid var(--border); border-radius: var(--radius); padding: 8px 8px 8px 12px;
              transition: border-color .15s; }
  #composer:focus-within { border-color: var(--accent); }
  #pills { display: flex; flex-wrap: wrap; gap: 6px; margin-bottom: 6px; }
  .pill { font-size: 11px; padding: 2px 8px; border-radius: 10px;
          background: var(--field); border: 1px solid var(--border); color: var(--muted); }
  #inp { flex: 1; resize: none; background: transparent; color: var(--vscode-input-foreground);
         border: none; outline: none; font: inherit; font-size: 13px; line-height: 1.5;
         max-height: 140px; padding: 4px 0; }
  #inp::placeholder { color: var(--muted); }
  #send { flex: none; width: 30px; height: 30px; border-radius: 50%; border: none; cursor: pointer;
          background: var(--vscode-button-background); color: var(--vscode-button-foreground);
          display: grid; place-items: center; transition: opacity .15s; }
  #send:hover { opacity: .88; }
  #send svg { width: 16px; height: 16px; }
  #send .spin { display: none; }
  body:has(#busy:checked) #send { pointer-events: none; }
  body:has(#busy:checked) #send .arrow { display: none; }
  body:has(#busy:checked) #send .spin { display: block; animation: spin .8s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }

  /* Status bar + hint */
  .ic { width: 13px; height: 13px; flex: none; }
  #statusbar { display: flex; align-items: center; gap: 10px; flex-wrap: wrap;
               font-size: 11px; color: var(--muted); padding: 0 4px 8px; }
  .sb { display: inline-flex; align-items: center; gap: 4px; }
  .ctxbar { width: 40px; height: 4px; border-radius: 2px; background: var(--code-bg); overflow: hidden; }
  #ctxfill { display: block; height: 100%; width: 0%; background: var(--ok); transition: width .3s, background .3s; }
  #statusbar #stat { margin-left: auto; }
  #hint { display: flex; gap: 12px; flex-wrap: wrap; font-size: 11px; color: var(--muted); padding: 6px 4px 0; }
  kbd { font-family: var(--vscode-editor-font-family); font-size: 10px; background: var(--code-bg);
        border: 1px solid var(--border); border-radius: 4px; padding: 0 4px; }
  .prompt-back { position: fixed; inset: 0; background: rgba(0,0,0,.45); display: flex;
        align-items: center; justify-content: center; z-index: 50; }
  .prompt-box { background: var(--vscode-editorWidget-background, var(--code-bg)); border: 1px solid var(--border);
        border-radius: 8px; padding: 14px; width: min(88%, 460px); display: flex; flex-direction: column; gap: 8px;
        box-shadow: 0 8px 30px rgba(0,0,0,.4); }
  .prompt-box.perm { border-color: var(--danger); }
  .prompt-title { font-size: 13px; font-weight: 600; }
  .prompt-detail { margin: 0; padding: 8px; background: var(--code-bg); border-radius: 6px; font-size: 12px;
        white-space: pre-wrap; word-break: break-word; max-height: 140px; overflow: auto; }
  .prompt-opt { text-align: left; padding: 7px 10px; border: 1px solid var(--border); border-radius: 6px;
        background: var(--vscode-button-secondaryBackground, transparent); color: var(--fg); cursor: pointer; font-size: 12px; }
  .prompt-opt:hover, .prompt-opt:focus { border-color: var(--accent); outline: none; }
  .prompt-row { display: flex; gap: 6px; align-items: center; }
  .prompt-row .prompt-opt { white-space: nowrap; }
  .prompt-in { flex: 1; min-width: 0; padding: 6px 8px; border: 1px solid var(--border); border-radius: 6px;
        background: var(--code-bg); color: var(--fg); font-family: var(--vscode-editor-font-family); font-size: 12px; }
</style>
</head>
<body>
  <svg width="0" height="0" style="position:absolute" aria-hidden="true"><defs>
    <symbol id="i-server" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="20" height="8" x="2" y="2" rx="2"/><rect width="20" height="8" x="2" y="14" rx="2"/><line x1="6" x2="6.01" y1="6" y2="6"/><line x1="6" x2="6.01" y1="18" y2="18"/></symbol>
    <symbol id="i-cpu" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="16" height="16" x="4" y="4" rx="2"/><rect width="6" height="6" x="9" y="9" rx="1"/><path d="M15 2v2"/><path d="M15 20v2"/><path d="M2 15h2"/><path d="M2 9h2"/><path d="M20 15h2"/><path d="M20 9h2"/><path d="M9 2v2"/><path d="M9 20v2"/></symbol>
    <symbol id="i-gauge" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/></symbol>
    <symbol id="i-clock" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></symbol>
    <symbol id="i-wrench" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/></symbol>
    <symbol id="i-plus" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/><path d="M12 5v14"/></symbol>
    <symbol id="i-gear" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/></symbol>
    <symbol id="i-close" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></symbol>
  </defs></svg>

  <input type="checkbox" id="busy" />

  <div id="topbar">
    <span id="title" title="Current chat">New chat</span>
    <span class="tb-actions">
      <button id="btn-history" class="tb-btn" title="Chat history"><svg class="ic"><use href="#i-clock"/></svg></button>
      <button id="btn-settings" class="tb-btn" title="Settings"><svg class="ic"><use href="#i-gear"/></svg></button>
      <button id="btn-new" class="tb-btn" title="New chat"><svg class="ic"><use href="#i-plus"/></svg></button>
    </span>
  </div>

  <div id="history" hidden>
    <div class="hist-head"><span>Chats — this project</span><button id="hist-close" class="tb-btn" title="Close"><svg class="ic"><use href="#i-close"/></svg></button></div>
    <div id="hist-list"></div>
  </div>

  <div id="settings-panel" hidden>
    <div class="hist-head"><span>Settings — this project</span><button id="set-close" class="tb-btn" title="Close"><svg class="ic"><use href="#i-close"/></svg></button></div>
    <div id="set-body">
      <div class="set-sec">
        <div class="set-sec-h">Account</div>
        <div class="set-note">Provider, key and model are stored globally in ~/.sirbone/.env (chmod 600) and shared with the CLI and TUI — this is the quick way to switch, e.g. from GLM to Groq.</div>
        <div class="set-note" id="cur-account">Active: —</div>
        <button id="btn-signin" class="set-link">＋ Sign in / switch provider</button>
      </div>
      <div class="set-sec">
        <div class="set-sec-h">Options</div>
        <div class="set-note">Model, thinking budget, grounding and the other scalar options live in VS Code settings.</div>
        <button id="open-vs-settings" class="set-link">Open VS Code settings →</button>
      </div>
      <div class="set-sec">
        <div class="set-sec-h">Skills <span class="set-hint">applies next run</span></div>
        <div class="set-note">Toggle which installed skills the agent may load in this project.</div>
        <div id="set-skills" class="set-list"></div>
      </div>
      <div class="set-sec">
        <div class="set-sec-h">MCP servers <span class="set-hint">applies next run</span></div>
        <div class="set-note">Toggle which MCP servers (from ~/.sirbone/mcp.json) start for this project.</div>
        <div id="set-mcp" class="set-list"></div>
      </div>
    </div>
  </div>

  <div id="log">
    <div id="empty">
      <img src="${a.icon}" alt="Sir Bone" />
      <h1>Sir Bone</h1>
      <p>Drive the sirbone agent on this workspace — ask about the code, request edits, run commands.</p>
      <div class="chips">
        <button class="chip" data-p="Explain the structure of this project">Explain project</button>
        <button class="chip" data-p="Find and fix a bug in the code">Find a bug</button>
        <button class="chip" data-p="Write tests for the main module">Write tests</button>
      </div>
    </div>
    <div id="end"></div>
  </div>

  <div id="bar">
    <div id="statusbar">
      <span class="sb" title="Provider"><svg class="ic"><use href="#i-server"/></svg><span id="provider">—</span></span>
      <span class="sb" title="Model"><svg class="ic"><use href="#i-cpu"/></svg><span id="model">—</span></span>
      <span class="sb" title="Context usage (peak vs window)"><svg class="ic"><use href="#i-gauge"/></svg><span class="ctxbar"><span id="ctxfill"></span></span><span id="ctxpct">—</span></span>
      <span class="sb" title="5-hour quota window"><svg class="ic"><use href="#i-clock"/></svg><span id="quota">—</span></span>
      <span id="stat"></span>
    </div>
    <div id="composer-wrap">
      <div id="suggest" hidden></div>
      <div id="pills" hidden></div>
      <div id="composer">
        <textarea id="inp" rows="1" placeholder="Ask sirbone…   ( / commands · @ files · ⌘V image )"></textarea>
        <button id="send" title="Send (Enter)">
          <svg class="arrow" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5"/><path d="M5 12l7-7 7 7"/></svg>
          <svg class="spin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M12 3a9 9 0 1 0 9 9"/></svg>
        </button>
      </div>
    </div>
    <div id="hint">
      <span><kbd>/</kbd> commands</span>
      <span><kbd>@</kbd> files</span>
      <span><kbd>Enter</kbd> send</span>
      <span><kbd>⇧↵</kbd> newline</span>
    </div>
  </div>

  <template id="t-user"><div class="row user"></div></template>
  <template id="t-assistant"><div class="row assistant"></div></template>
  <template id="t-thinking"><div class="row thinking"></div></template>
  <template id="t-err"><div class="row err"></div></template>
  <template id="t-tool"><div class="row tool">
    <div class="t-head"><svg class="ic"><use href="#i-wrench"/></svg><span class="t-name"></span><span class="t-meta"></span></div>
    <div class="t-io">
      <div class="t-line t-inline"><span class="t-lbl">IN</span><span class="t-in"></span></div>
      <div class="t-line t-outline"><span class="t-lbl">OUT</span><span class="t-out"></span></div>
    </div>
  </div></template>

<script nonce="${cspNonce}" src="${a.marked}"></script>
<script nonce="${cspNonce}" src="${a.purify}"></script>
<script nonce="${cspNonce}" src="${a.hljs}"></script>
<script nonce="${cspNonce}" src="${a.mermaid}"></script>
<script nonce="${cspNonce}">
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const log = $('log'), end = $('end'), busy = $('busy'), inp = $('inp'), stat = $('stat'), send = $('send');
  // Chatbot-style auto-scroll: stay pinned to the newest text while streaming,
  // but stop following if the user scrolls up (resume when they return to bottom).
  let autoScroll = true;
  log.addEventListener('scroll', () => { autoScroll = (log.scrollHeight - log.scrollTop - log.clientHeight) < 80; });
  const stick = () => { if (autoScroll) log.scrollTop = log.scrollHeight; };
  const ctxfill = $('ctxfill'), ctxpct = $('ctxpct'), quotaEl = $('quota'), providerEl = $('provider'), modelEl = $('model');
  const clone = (kind) => $('t-' + kind).content.firstElementChild.cloneNode(true);
  const fmt = (n) => n >= 1000 ? (n / 1000).toFixed(n >= 10000 ? 0 : 1) + 'k' : String(n);
  const hhmm = (ts) => { const m = String(ts || '').match(/T(\\d\\d:\\d\\d)/); return m ? m[1] : ''; };

  const light = document.body.classList.contains('vscode-light');
  $('hljs-light').disabled = !light;
  $('hljs-dark').disabled = light;
  marked.setOptions({ gfm: true, breaks: false });
  try { mermaid.initialize({ startOnLoad: false, securityLevel: 'loose', theme: light ? 'default' : 'dark' }); } catch (e) {}
  // Render each diagram on its own; a malformed one (e.g. the model omitted the
  // 'flowchart'/'graph' keyword) falls back to its source instead of mermaid's bomb.
  let mmSeq = 0;
  const renderMermaid = async (nodes) => {
    for (const node of nodes) {
      const src = node.textContent;
      const id = 'mm-' + (mmSeq++);
      try {
        const { svg } = await mermaid.render(id, src);
        node.innerHTML = svg;
      } catch (e) {
        // On error mermaid leaves an orphaned element (the "bomb") attached to
        // document.body — remove it, then show the source in-place instead.
        document.getElementById(id)?.remove();
        document.getElementById('d' + id)?.remove();
        const pre = document.createElement('pre'); pre.className = 'mmfail';
        const code = document.createElement('code'); code.textContent = src;
        pre.appendChild(code); node.replaceWith(pre);
      }
    }
  };

  const setCtx = (pct) => {
    ctxpct.textContent = pct + '%';
    ctxfill.style.width = pct + '%';
    ctxfill.style.background = pct >= 88 ? 'var(--danger)' : pct >= 70 ? 'var(--warn)' : 'var(--ok)';
  };
  const human = (ms) => { const m = Math.max(0, Math.round(ms / 60000)); const h = Math.floor(m / 60); return h > 0 ? h + 'h ' + (m % 60) + 'm' : m + 'm'; };
  const setQuota = (q) => { quotaEl.textContent = q ? (human(q.leftMs) + ' left · ' + q.pct + '%') : 'idle'; };

  const add = (kind, text, extra) => { const el = clone(kind); if (extra) el.classList.add(extra); el.textContent = text; end.before(el); stick(); };

  const renderMarkdownInto = (el, text) => {
    el.innerHTML = DOMPurify.sanitize(marked.parse(text || ''));
    el.querySelectorAll('code.language-mermaid').forEach((code) => {
      const pre = document.createElement('pre'); pre.className = 'mermaid'; pre.textContent = code.textContent;
      (code.closest('pre') || code).replaceWith(pre);
    });
    el.querySelectorAll('pre code').forEach((c) => { if (!c.closest('.mermaid')) { try { hljs.highlightElement(c); } catch (e) {} } });
    const nodes = el.querySelectorAll('.mermaid');
    if (nodes.length) renderMermaid(nodes);
  };
  const addAssistant = (text) => { const el = clone('assistant'); renderMarkdownInto(el, text); end.before(el); stick(); };

  // Live streaming (typewriter): append raw text into the active bubble as chunks
  // arrive, then render it as markdown once the block ends (a tool/thinking/result).
  let streamEl = null, streamBuf = '', streamKind = null;
  const finalizeStream = () => {
    if (!streamEl) return;
    streamEl.classList.remove('streaming');
    if (streamKind === 'assistant') renderMarkdownInto(streamEl, streamBuf);
    streamEl = null; streamBuf = ''; streamKind = null;
    stick();
  };
  const streamAppend = (kind, text) => {
    if (streamKind && streamKind !== kind) finalizeStream();
    if (!streamEl) { streamEl = clone(kind); streamEl.classList.add('streaming'); streamBuf = ''; streamKind = kind; end.before(streamEl); }
    streamBuf += text;
    streamEl.textContent = streamBuf;
    stick();
  };

  const tools = {};
  const sumInput = (input) => {
    if (input == null) return '';
    if (typeof input !== 'object') return String(input);
    if (input.path) return input.path + (input.limit ? ' :' + input.limit : '');
    if (input.command) return input.command;
    if (input.pattern) return input.pattern;
    const s = JSON.stringify(input); return s.length > 140 ? s.slice(0, 140) + '…' : s;
  };
  const fillOut = (box, m) => {
    // Todo boxes already show the checklist as their body — only update timing.
    if (box._todo) {
      if (box._ts && m.ts) { const d = (Date.parse(m.ts) - box._ts) / 1000; if (d >= 0) box.querySelector('.t-meta').textContent = hhmm(m.ts) + ' · ' + d.toFixed(1) + 's'; }
      stick(); return;
    }
    const out = box.querySelector('.t-out'); out.innerHTML = '';
    box.classList.toggle('err', !!m.isError);
    const text = String(m.content == null ? '' : m.content);
    const lines = text.split('\\n');
    const body = document.createElement('div'); body.className = 't-outbody';
    if (lines.length > 8) {
      body.textContent = lines.slice(0, 8).join('\\n');
      const more = document.createElement('button'); more.className = 't-expand';
      more.textContent = '+' + (lines.length - 8) + ' more lines';
      more.addEventListener('click', () => { body.textContent = text; more.remove(); });
      out.append(body, more);
    } else { body.textContent = text; out.append(body); }
    if (box._ts && m.ts) { const d = (Date.parse(m.ts) - box._ts) / 1000; if (d >= 0) box.querySelector('.t-meta').textContent = hhmm(m.ts) + ' · ' + d.toFixed(1) + 's'; }
    stick();
  };
  const addTool = (m) => {
    const box = clone('tool');
    box.querySelector('.t-name').textContent = m.name;
    box.querySelector('.t-meta').textContent = hhmm(m.ts);
    // The todo tool renders as a live checklist instead of a raw IN row.
    const items = m.name === 'todo' && Array.isArray(m.input && m.input.todos) ? m.input.todos : null;
    if (items) {
      box._todo = true;
      const inEl = box.querySelector('.t-in'); inEl.textContent = '';
      const list = document.createElement('div'); list.className = 'todo-list';
      for (const t of items) {
        const st = t.status === 'completed' || t.status === 'in_progress' ? t.status : 'pending';
        const row = document.createElement('div'); row.className = 'todo-item ' + st;
        const mark = document.createElement('span'); mark.className = 'tmark';
        mark.textContent = st === 'completed' ? '✔' : st === 'in_progress' ? '❯' : '☐';
        row.append(mark, document.createTextNode(String(t.content == null ? '' : t.content)));
        list.append(row);
      }
      inEl.append(list);
    } else {
      box.querySelector('.t-in').textContent = sumInput(m.input);
    }
    box._ts = Date.parse(m.ts || '');
    end.before(box);
    if (m.id) tools[m.id] = box;
    stick();
  };
  const addResult = (m) => {
    const box = (m.id && tools[m.id]) || null;
    if (box) { fillOut(box, m); return; }
    const b = clone('tool');
    b.querySelector('.t-name').textContent = 'result';
    b.querySelector('.t-inline').remove();
    end.before(b);
    fillOut(b, m);
  };

  const grow = () => { inp.style.height = 'auto'; inp.style.height = Math.min(inp.scrollHeight, 140) + 'px'; };
  const submit = (raw) => {
    const text = (raw != null ? raw : inp.value).trim();
    if (!text || busy.checked) return;
    autoScroll = true;
    add('user', text);
    if (titleEl.textContent === 'New chat') setTitle(text.slice(0, 60));
    inp.value = ''; grow(); clearPills();
    busy.checked = true; stat.textContent = 'running…';
    vscode.postMessage({ type: 'ask', text });
  };

  // ── Slash commands (/) and file mentions (@) ──
  const suggest = $('suggest');
  const SLASH = [
    { cmd: '/new', desc: 'Start a new chat' },
    { cmd: '/clear', desc: 'Clear this chat' },
    { cmd: '/history', desc: 'Open chat history' },
    { cmd: '/historia', desc: 'Continue from prior project work' },
    { cmd: '/settings', desc: 'Open settings' },
    { cmd: '/help', desc: 'List commands' },
  ];
  let files = [], skills = [], sgItems = [], sgSel = -1, sgMode = null;
  const hideSg = () => { suggest.hidden = true; sgItems = []; sgSel = -1; sgMode = null; };
  const updateSel = () => { [...suggest.children].forEach((c, i) => c.classList.toggle('sel', i === sgSel)); if (sgSel >= 0) suggest.children[sgSel]?.scrollIntoView({ block: 'nearest' }); };
  const showSg = (items, mode) => {
    sgItems = items; sgMode = mode; sgSel = items.length ? 0 : -1;
    if (!items.length) { hideSg(); return; }
    suggest.innerHTML = '';
    items.forEach((it, i) => {
      const d = document.createElement('div'); d.className = 'sg-item' + (i === 0 ? ' sel' : '');
      if (mode === 'slash') {
        const a = document.createElement('span'); a.className = 'sg-cmd'; a.textContent = it.cmd;
        d.append(a);
        if (it.skill) { const tag = document.createElement('span'); tag.className = 'sg-tag' + (it.enabled ? '' : ' off'); tag.textContent = it.enabled ? 'skill' : 'skill · off'; d.append(tag); }
        if (it.desc) { const b = document.createElement('span'); b.className = 'sg-desc'; b.textContent = it.desc; d.append(b); }
      }
      else { const a = document.createElement('span'); a.className = 'sg-path'; a.textContent = it; d.append(a); }
      d.addEventListener('mousedown', (e) => { e.preventDefault(); accept(i); });
      suggest.append(d);
    });
    suggest.hidden = false;
  };
  const fileToken = () => {
    const before = inp.value.slice(0, inp.selectionStart);
    const m = before.match(/(^|\\s)@(\\S*)$/);
    return m ? { q: m[2], start: inp.selectionStart - m[2].length } : null;
  };
  const refreshSg = () => {
    const v = inp.value;
    if (v.startsWith('/') && !v.includes(' ')) {
      if (!skills.length) vscode.postMessage({ type: 'skills' });
      const q = v.slice(1).toLowerCase();
      const cmds = SLASH.filter((c) => c.cmd.slice(1).startsWith(q));
      const sk = skills.filter((s) => s.name.toLowerCase().includes(q)).map((s) => ({ cmd: '/' + s.name, desc: s.desc, skill: true, enabled: s.enabled }));
      return showSg(cmds.concat(sk), 'slash');
    }
    const ft = fileToken();
    if (ft) {
      if (!files.length) vscode.postMessage({ type: 'files' });
      const q = ft.q.toLowerCase();
      return showSg(files.filter((f) => f.toLowerCase().includes(q)).slice(0, 50), 'file');
    }
    hideSg();
  };
  const runSlash = (cmd) => {
    if (cmd === '/historia') {
      inp.value = '/historia ';
      hideSg(); grow(); inp.focus();
      const p = inp.value.length; inp.setSelectionRange(p, p);
      return;
    }
    inp.value = ''; grow(); hideSg();
    if (cmd === '/new' || cmd === '/clear') vscode.postMessage({ type: 'reset' });
    else if (cmd === '/history') { vscode.postMessage({ type: 'history' }); histOverlay.hidden = false; }
    else if (cmd === '/settings') vscode.postMessage({ type: 'settings' });
    else if (cmd === '/help') addAssistant('**Commands**\\n\\n- \`/new\` — start a new chat\\n- \`/clear\` — clear this chat\\n- \`/history\` — open chat history\\n- \`/historia [date or topic]\` — continue prior project work\\n- \`/settings\` — open settings\\n\\nType \`@\` to mention a project file.');
  };
  const accept = (i) => {
    const it = sgItems[i]; if (it == null) return;
    if (sgMode === 'slash') {
      if (it.skill) {
        inp.value = 'Use the "' + it.cmd.slice(1) + '" skill: ';
        hideSg(); grow(); inp.focus();
        const p = inp.value.length; inp.setSelectionRange(p, p);
        return;
      }
      return runSlash(it.cmd);
    }
    const ft = fileToken(); if (!ft) return hideSg();
    const pos = inp.selectionStart, v = inp.value;
    inp.value = v.slice(0, ft.start) + it + ' ' + v.slice(pos);
    const caret = ft.start + it.length + 1;
    hideSg(); grow(); inp.focus(); inp.setSelectionRange(caret, caret);
  };

  // Image paste. The webview has no clipboard-read API, but a real paste event
  // carries the bitmap, so the gesture stays the user's and nothing is polled.
  const pills = $('pills');
  inp.addEventListener('paste', (e) => {
    const file = [...(e.clipboardData?.items ?? [])]
      .filter((it) => it.kind === 'file' && it.type.startsWith('image/'))
      .map((it) => it.getAsFile())[0];
    if (!file) return; // plain text paste — let the textarea handle it
    e.preventDefault();
    const r = new FileReader();
    r.onload = () => {
      const data = String(r.result).split(',')[1] || '';
      const ext = (file.type.split('/')[1] || 'png').replace('jpeg', 'jpg');
      vscode.postMessage({ type: 'attach', name: 'paste.' + ext, data });
    };
    r.readAsDataURL(file);
  });
  const addPill = (label) => {
    const p = document.createElement('span');
    p.className = 'pill';
    p.textContent = '📎 ' + label;
    pills.append(p);
    pills.hidden = false;
  };
  const clearPills = () => { pills.innerHTML = ''; pills.hidden = true; };

  inp.addEventListener('input', () => { grow(); refreshSg(); });
  inp.addEventListener('keydown', (e) => {
    if (!suggest.hidden) {
      if (e.key === 'ArrowDown') { e.preventDefault(); sgSel = Math.min(sgSel + 1, sgItems.length - 1); return updateSel(); }
      if (e.key === 'ArrowUp') { e.preventDefault(); sgSel = Math.max(sgSel - 1, 0); return updateSel(); }
      if (e.key === 'Enter' || e.key === 'Tab') { e.preventDefault(); return accept(sgSel); }
      if (e.key === 'Escape') { e.preventDefault(); return hideSg(); }
    }
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); submit(); }
  });
  send.addEventListener('click', () => submit());
  document.querySelectorAll('.chip').forEach((c) => c.addEventListener('click', () => submit(c.dataset.p)));

  // ── Top bar + history ──
  const titleEl = $('title'), histOverlay = $('history'), histList = $('hist-list');
  const setTitle = (t) => { titleEl.textContent = t || 'New chat'; };
  const clearChat = () => { log.querySelectorAll('.row').forEach((n) => n.remove()); for (const k in tools) delete tools[k]; };
  const rel = (ts) => {
    const s = Math.max(0, (Date.now() - ts) / 1000);
    if (s < 60) return 'just now';
    const m = Math.floor(s / 60); if (m < 60) return m + 'm';
    const h = Math.floor(m / 60); if (h < 24) return h + 'h';
    const d = Math.floor(h / 24); if (d < 7) return d + 'd';
    return Math.floor(d / 7) + 'w';
  };
  const renderRow = (m) => {
    if (m.type === 'user') add('user', m.text);
    else if (m.type === 'assistant') addAssistant(m.text);
    else if (m.type === 'thinking') { if (m.text) add('thinking', m.text); }
    else if (m.type === 'tool') addTool(m);
    else if (m.type === 'toolresult') addResult(m);
  };
  $('btn-history').addEventListener('click', () => { vscode.postMessage({ type: 'history' }); histOverlay.hidden = false; });
  $('hist-close').addEventListener('click', () => { histOverlay.hidden = true; });
  $('btn-new').addEventListener('click', () => { vscode.postMessage({ type: 'reset' }); });

  // Settings panel (skills + MCP checkbox lists, like the TUI subsections)
  const settingsPanel = $('settings-panel');
  const updateAccount = () => { const el = $('cur-account'); if (el) el.textContent = 'Active: ' + (providerEl.textContent || '—') + ' · ' + (modelEl.textContent || '—'); };
  const openSettings = () => { vscode.postMessage({ type: 'config' }); settingsPanel.hidden = false; histOverlay.hidden = true; updateAccount(); };
  $('btn-settings').addEventListener('click', openSettings);
  $('set-close').addEventListener('click', () => { settingsPanel.hidden = true; });
  $('open-vs-settings').addEventListener('click', () => vscode.postMessage({ type: 'settings' }));
  $('btn-signin').addEventListener('click', () => vscode.postMessage({ type: 'signIn' }));
  const renderConfig = (skillsList, mcp) => {
    const build = (host, items, kind, emptyMsg) => {
      host.innerHTML = '';
      if (!items.length) { const e = document.createElement('div'); e.className = 'set-empty'; e.textContent = emptyMsg; host.append(e); return; }
      for (const it of items) {
        const row = document.createElement('div'); row.className = 'set-row' + (it.enabled ? ' on' : '');
        const box = document.createElement('span'); box.className = 'set-box'; box.textContent = it.enabled ? '✓' : '';
        const main = document.createElement('div'); main.className = 'set-main';
        const nm = document.createElement('span'); nm.className = 'set-name'; nm.textContent = it.name;
        const ds = document.createElement('span'); ds.className = 'set-desc'; ds.textContent = kind === 'skill' ? (it.desc || '') : (it.cmd || '');
        main.append(nm, ds); row.append(box, main);
        row.addEventListener('click', () => vscode.postMessage({ type: kind === 'skill' ? 'toggleSkill' : 'toggleMcp', name: it.name, on: !it.enabled }));
        host.append(row);
      }
    };
    build($('set-skills'), skillsList, 'skill', 'No skills installed (~/.sirbone/skills).');
    build($('set-mcp'), mcp, 'mcp', 'No MCP servers in ~/.sirbone/mcp.json.');
  };
  addEventListener('keydown', (e) => {
    if ((e.ctrlKey || e.metaKey) && (e.key === 'n' || e.key === 'N')) { e.preventDefault(); vscode.postMessage({ type: 'reset' }); }
    else if ((e.ctrlKey || e.metaKey) && e.key === ',') { e.preventDefault(); openSettings(); }
  });

  // Modal for a permission gate / ask_user question streamed from the agent.
  // Answers post {type:'promptReply', id, index?, text?} back to the extension,
  // which writes it to sirbone's stdin control channel.
  function showPrompt(id, p) {
    document.querySelectorAll('.prompt-back').forEach((n) => n.remove());
    const isPerm = p.kind && p.kind.type === 'permission';
    const back = document.createElement('div'); back.className = 'prompt-back';
    const box = document.createElement('div'); box.className = 'prompt-box' + (isPerm ? ' perm' : '');
    const h = document.createElement('div'); h.className = 'prompt-title';
    h.textContent = p.title || (isPerm ? 'Permission required' : 'Question');
    box.append(h);
    if (p.detail) { const d = document.createElement('pre'); d.className = 'prompt-detail'; d.textContent = p.detail; box.append(d); }
    const answer = (index, text) => { vscode.postMessage({ type: 'promptReply', id, index, text }); back.remove(); };
    (p.options || []).forEach((opt, i) => {
      const b = document.createElement('button'); b.className = 'prompt-opt'; b.textContent = opt;
      if (isPerm && i === 1) {
        // "Allow always": editable glob persisted to permissions.allow.
        const row = document.createElement('div'); row.className = 'prompt-row';
        const inp = document.createElement('input'); inp.className = 'prompt-in';
        inp.value = (p.kind && p.kind.suggested_glob) || '';
        b.addEventListener('click', () => answer(i, inp.value.trim() || null));
        row.append(b, inp); box.append(row);
      } else {
        b.addEventListener('click', () => answer(i, null));
        box.append(b);
      }
    });
    if (p.allow_free_text) {
      const row = document.createElement('div'); row.className = 'prompt-row';
      const inp = document.createElement('input'); inp.className = 'prompt-in';
      inp.placeholder = isPerm ? 'Deny & tell the agent what to do…' : 'Other…';
      const b = document.createElement('button'); b.className = 'prompt-opt'; b.textContent = isPerm ? 'Deny + feedback' : 'Other';
      const go = () => answer(null, inp.value.trim() || null);
      b.addEventListener('click', go);
      inp.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') { ev.preventDefault(); go(); } });
      row.append(inp, b); box.append(row);
    }
    back.append(box); document.body.append(back);
    const first = box.querySelector('.prompt-opt'); if (first) first.focus();
  }

  addEventListener('message', (e) => {
    const m = e.data;
    if (m.type === 'stream_text') streamAppend('assistant', m.text);
    else if (m.type === 'stream_thinking') streamAppend('thinking', m.text);
    else if (m.type === 'stream_ctx') setCtx(m.ctxPct);
    else if (m.type === 'assistant') addAssistant(m.text);
    else if (m.type === 'thinking') { if (m.text) add('thinking', m.text); }
    else if (m.type === 'tool') { finalizeStream(); addTool(m); }
    else if (m.type === 'toolresult') addResult(m);
    else if (m.type === 'prompt') { finalizeStream(); showPrompt(m.id, m.prompt || {}); }
    else if (m.type === 'error') { finalizeStream(); add('err', m.text); busy.checked = false; stat.textContent = ''; }
    else if (m.type === 'attached') addPill(m.label);
    else if (m.type === 'status') { setQuota(m.quota); }
    else if (m.type === 'meta') { if (m.provider) providerEl.textContent = m.provider; if (m.model) modelEl.textContent = m.model; updateAccount(); }
    else if (m.type === 'files') { files = m.list || []; refreshSg(); }
    else if (m.type === 'skills') { skills = m.list || []; if (!suggest.hidden && sgMode === 'slash') refreshSg(); }
    else if (m.type === 'config') { skills = m.skills || skills; renderConfig(m.skills || [], m.mcp || []); }
    else if (m.type === 'sessions') {
      histList.innerHTML = '';
      if (!m.list.length) { const e = document.createElement('div'); e.className = 'hist-empty'; e.textContent = 'No previous chats in this project.'; histList.append(e); }
      for (const s of m.list) {
        const it = document.createElement('div'); it.className = 'hist-item';
        const t = document.createElement('span'); t.className = 'hist-title'; t.textContent = s.title;
        const tm = document.createElement('span'); tm.className = 'hist-time'; tm.textContent = rel(s.ts);
        it.append(t, tm);
        it.addEventListener('click', () => vscode.postMessage({ type: 'open', id: s.id }));
        histList.append(it);
      }
    }
    else if (m.type === 'load') {
      clearChat(); setTitle(m.title);
      for (const ev of m.events) renderRow(ev);
      histOverlay.hidden = true; busy.checked = false; stat.textContent = '';
    }
    else if (m.type === 'done') {
      finalizeStream();
      const u = m.usage;
      stat.textContent = u ? (fmt(u.input_tokens) + ' in · ' + fmt(u.output_tokens) + ' out · ' + u.tool_calls + ' tools') : '';
      if (m.ctxPct != null) setCtx(m.ctxPct);
      setQuota(m.quota);
      busy.checked = false;
    } else if (m.type === 'cleared') {
      streamEl = null; streamBuf = ''; streamKind = null;
      clearChat(); setTitle('New chat'); histOverlay.hidden = true;
      busy.checked = false; stat.textContent = '';
      ctxpct.textContent = '—'; ctxfill.style.width = '0%'; quotaEl.textContent = '—';
    }
  });

  vscode.postMessage({ type: 'ready' });
</script>
</body>
</html>`;
}

export function deactivate() {}

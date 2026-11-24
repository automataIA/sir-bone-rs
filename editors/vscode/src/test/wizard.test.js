// Integration test headless (xvfb-run + code --extensionTestsPath): pilota il
// wizard signIn() di extension.ts sostituendo le dialog di VS Code con risposte
// automatiche, esattamente come farebbe un utente: token incollato (presa dal
// ~/.sirbone/.env reale), provider scelto dal QuickPick, modello scelto dal
// dropdown popolato dalla lista live. Verifica che il dropdown riceva ≥1 modello
// e che alla fine ~/.sirbone/.env sia coerente.
const vscode = require("vscode");
const fs = require("fs");
const os = require("os");
const path = require("path");

async function run() {
  const envPath = path.join(os.homedir(), ".sirbone", ".env");
  const env = Object.fromEntries(
    fs.readFileSync(envPath, "utf8")
      .split("\n")
      .filter((l) => /^[A-Z_]+=/.test(l))
      .map((l) => [l.slice(0, l.indexOf("=")), l.slice(l.indexOf("=") + 1)])
  );
  const token = env.OPENAI_API_KEY;
  const baseUrl = env.OPENAI_BASE_URL;
  if (!token) throw new Error("no token in ~/.sirbone/.env");

  // Il wizard deve usare il binario appena buildato, non ~/.cargo/bin/sirbone
  // (versione installata, senza --login-list-models).
  const cfg = vscode.workspace.getConfiguration("sirbone");
  await cfg.update("binaryPath", "/home/dio/sir-bone-rs/target/debug/sirbone", vscode.ConfigurationTarget.Global);

  const picked = { provider: undefined, modelItems: undefined, model: undefined };
  const origInput = vscode.window.showInputBox;
  const origPick = vscode.window.showQuickPick;
  const origWarn = vscode.window.showWarningMessage;
  const origInfo = vscode.window.showInformationMessage;

  let step = 0;
  vscode.window.showInputBox = async (opts) => {
    step++;
    if (step === 1) return token; // 1. token (paste)
    if (step === 2) return baseUrl; // 3. base URL (conferma il prefill z.ai)
    throw new Error("unexpected input box step " + step);
  };
  vscode.window.showQuickPick = async (items, opts) => {
    if (opts?.placeHolder?.startsWith("Select your provider")) {
      // 2. provider: cerca la voce z.ai/GLM... qui il .env usa un endpoint
      // OpenAI-compatibile, quindi sceglie OpenAI-compat: usiamo Groq? No —
      // serve il provider che usa il nostro base URL: value "openai".
      picked.provider = items.find((i) => i.value?.value === "openai");
      return picked.provider;
    }
    // 4. dropdown modelli: registra gli items e scegli il model corrente
    picked.modelItems = items.map((i) => i.label);
    return { label: env.SIRBONE_MODEL };
  };
  vscode.window.showWarningMessage = async () => undefined;
  let infoMsg = "";
  vscode.window.showInformationMessage = async (m) => { infoMsg = m; return undefined; };

  try {
    await vscode.commands.executeCommand("sirbone.signIn");
  } finally {
    vscode.window.showInputBox = origInput;
    vscode.window.showQuickPick = origPick;
    vscode.window.showWarningMessage = origWarn;
    vscode.window.showInformationMessage = origInfo;
  }

  console.log("provider picked:", picked.provider?.label);
  console.log("models in dropdown:", picked.modelItems?.length);
  console.log("current model in dropdown:", (picked.modelItems || []).includes(env.SIRBONE_MODEL));
  console.log("final info message:", infoMsg);

  if (!picked.modelItems || picked.modelItems.length === 0)
    throw new Error("dropdown empty — live model list not delivered to the wizard");
  if (!(picked.modelItems || []).includes(env.SIRBONE_MODEL))
    throw new Error("current model missing from dropdown");
  if (!infoMsg.includes("credentials saved")) throw new Error("wizard did not complete: " + infoMsg);

  const after = fs.readFileSync(envPath, "utf8");
  if (!after.includes("SIRBONE_MODEL=" + env.SIRBONE_MODEL)) throw new Error("SIRBONE_MODEL not persisted");
  if (!after.includes("OPENAI_BASE_URL=" + baseUrl)) throw new Error("base URL not persisted");
  console.log("PASS: wizard end-to-end (token→provider→live dropdown→model→.env write)");
}

module.exports = { run };

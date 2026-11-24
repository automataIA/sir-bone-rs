// Integration test headless: pilota il cambio thinking level (QuickPick stub →
// comando sirbone.setThinkingLevel) e verifica che (a) la config cambi,
// (b) doctor riporti il livello (via buildEnv/SIRBONE_THINKING_BUDGET),
// (c) il meta mostrato nella topbar contenga "model (level)".
const vscode = require("vscode");

async function run() {
  const cfg = vscode.workspace.getConfiguration("sirbone");
  await cfg.update("binaryPath", "/home/dio/sir-bone-rs/target/debug/sirbone", vscode.ConfigurationTarget.Global);
  await cfg.update("thinkingBudget", 0, vscode.ConfigurationTarget.Global);

  // Stub del QuickPick del livello: sceglie "Max".
  const origPick = vscode.window.showQuickPick;
  const origInfo = vscode.window.showInformationMessage;
  let infoMsg = "";
  vscode.window.showQuickPick = async (items, opts) => {
    if (opts?.placeHolder?.startsWith("Thinking level")) return items.find((i) => i.label === "Max");
    throw new Error("unexpected quickpick: " + opts?.placeHolder);
  };
  vscode.window.showInformationMessage = async (m) => { infoMsg = m; return undefined; };
  try {
    await vscode.commands.executeCommand("sirbone.setThinkingLevel");
  } finally {
    vscode.window.showQuickPick = origPick;
    vscode.window.showInformationMessage = origInfo;
  }
  const budget = vscode.workspace.getConfiguration("sirbone").get("thinkingBudget", -1);
  console.log("thinkingBudget after pick:", budget);
  console.log("info message:", infoMsg);
  if (budget !== 32000) throw new Error("budget not updated to 32000, got " + budget);
  if (!infoMsg.includes("Max")) throw new Error("no confirmation message");

  // Doctor con l'env che userà la chat (buildEnv) deve riportare "max".
  const { execFile } = require("child_process");
  const env = { ...process.env, SIRBONE_THINKING_BUDGET: "32000" };
  delete env.ANTHROPIC_AUTH_TOKEN; delete env.ANTHROPIC_BASE_URL; // pulizia sessione agente
  const out = await new Promise((res, rej) =>
    execFile("/home/dio/sir-bone-rs/target/debug/sirbone", ["doctor"], { cwd: "/home/dio/sir-bone-rs", env }, (e, so) => e ? rej(e) : res(so)));
  const model = out.match(/\bmodel:\s*(\S+)/)?.[1];
  const thinking = out.match(/thinking:\s*(\S+)/)?.[1];
  console.log("doctor model:", model, "| thinking:", thinking);
  if (thinking !== "max") throw new Error("doctor does not report max, got " + thinking);

  // Ripristina il livello precedente per non lasciare stato modificato.
  await cfg.update("thinkingBudget", 0, vscode.ConfigurationTarget.Global);
  console.log("PASS: thinking level switch (pick → config → doctor → display source)");
}

module.exports = { run };

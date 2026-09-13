# Sir Bone — VS Code extension

Pannello chat nativo che pilota il binario [`sirbone`](https://github.com/automataIA/sir-bone-rs) dentro VS Code.
Non tocca il crate Rust: l'host TS lancia `sirbone -p --output-format stream-json --input-format stream-json --session <path>`
per ogni turno e tail-a il file di sessione `.jsonl` per il progresso live.

## Requisiti

- `sirbone` installato su PATH/`~/.cargo/bin` con `cargo install --path . --locked`
  (in alternativa imposta `sirbone.binaryPath`).
- Una API key configurata come per la CLI (env di VS Code oppure `~/.sirbone/.env`).
- Node ≥ 18 per compilare l'estensione.

## Build & run (dev)

```bash
cd editors/vscode
npm install
npm run compile          # tsc -> out/extension.js
```

Poi in VS Code: `F5` (Extension Development Host) → Command Palette →
**"Sirbone: Open Chat"**.

## Impostazioni

| Setting | Default | Nota |
|---|---|---|
| `sirbone.binaryPath` | `"sirbone"` | Path/nome del binario; il default cerca anche `~/.cargo/bin`. |
| `sirbone.extraArgs` | `[]` | Es. `["--model","claude-opus-4-7"]`. |
| `sirbone.planMode` | `false` | Avvia ogni nuovo turno in Plan mode, passando automaticamente `SIRBONE_PLAN=1`. |

## Architettura (breve)

- **Host** (`src/extension.ts`): unico file TS. Genera un path di sessione
  deterministico (`~/.sirbone/projects/<slug>/sessions/<uuid>.jsonl`, stesso slug di
  sirbone), lancia il binario, legge le righe appese al `.jsonl` (`SessionEntry`) e le
  inoltra al webview; a fine processo legge il blob finale `{result,usage,session}`.
  I turni successivi riusano lo stesso file → contesto mantenuto.
- **Webview**: HTML+CSS+SVG, stile Claude Code/Codex. I messaggi assistant sono resi
  in **markdown** (`marked`) con **tabelle**, **code highlight** (`highlight.js`),
  **diagrammi mermaid** (fence ` ```mermaid `) e sanitize (`DOMPurify`). Le librerie
  sono **bundle locale** in `media/vendor/` (copiate da `scripts/copy-assets.mjs`,
  eseguito da `npm run compile`), caricate sotto **CSP nonce** — nessun CDN.
- **Status bar** con icone Lucide: provider · model (da `sirbone doctor`), ctx% (gauge
  verde→arancio→rosso), finestra quota 5h. **Shortcut**: `Ctrl/⌘ N` nuova sessione,
  `Ctrl ,` settings.

## Limiti (importanti)

- **Permessi headless.** In modalità `-p` sirbone non ha canale di conferma: ogni
  comando che richiederebbe conferma (`git push`/`reset`, bash distruttivo) viene
  **auto-negato** e l'errore appare in chat. Per abilitarli aggiungi glob in
  `permissions.allow` in `~/.sirbone/config.json` o
  `~/.sirbone/projects/<slug>/config.json`.
- **Granularità del progresso = turno**, non token: il `.jsonl` è scritto dopo ogni
  turno, quindi tool call e testo appaiono a blocchi, non carattere per carattere.
- **API key mancante** → `sirbone` esce con errore, mostrato in chat.

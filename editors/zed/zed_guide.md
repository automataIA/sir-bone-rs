# Guida: Sir Bone in Zed (agente ACP) — macOS · Linux · Windows · Windows+WSL

Sir Bone si integra nel **pannello Agent di Zed** come *external agent* parlando
**ACP (Agent Client Protocol)** — JSON-RPC 2.0 su stdio. Zed lancia il binario
`sirbone` come sottoprocesso: **niente pacchetto estensione da installare**, basta
il binario più una voce `agent_servers` nelle settings di Zed.

Cosa ottieni: risposte in streaming, card dei tool, prompt di permesso (allow
once / allow always / reject), resume delle sessioni — tutto nativo nel pannello.

> Panoramica rapida delle capacità e limiti noti: vedi
> [`editors/zed/README.md`](./README.md).

---

## 0. Concetti validi per tutti gli OS

Ti servono **tre cose**, in qualsiasi sistema:

1. **Il binario `sirbone`** (nativo per l'ambiente in cui gira l'agente).
2. **Una key provider**, via `sirbone login` (scrive `~/.sirbone/.env`, 0600) o via
   `env` nella config di Zed.
3. **La voce `agent_servers`** nelle settings di Zed.

La config di Sir Bone vive in `~/.sirbone/` — cioè `$HOME/.sirbone` su
macOS/Linux e `%USERPROFILE%\.sirbone` su Windows (risolto da `dirs::home_dir()`).

**Regola d'oro sulla topologia:** l'agente deve girare **nello stesso ambiente in
cui vivono i file del progetto**, perché Sir Bone fa I/O sul filesystem con path
nativi e non traduce tra Windows e Linux. Quindi:

| Dove apri il progetto in Zed | Dove deve girare `sirbone` |
|---|---|
| macOS | binario macOS |
| Linux | binario Linux |
| Windows (filesystem Windows) | `sirbone.exe` Windows |
| **WSL (progetto in `\\wsl$` / dentro la distro)** | **binario Linux dentro WSL** (via Zed remote-SSH) |

---

## 1. Installare `sirbone`

Scegli **un** metodo. Tutti i comandi valgono nell'ambiente-target (su Windows+WSL
= *dentro* WSL, non in PowerShell).

### Binario precompilato (senza toolchain Rust)

```bash
# macOS / Linux / WSL
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/automataIA/sir-bone-rs/releases/latest/download/sir-bone-rs-installer.sh | sh
```

```powershell
# Windows nativo (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/automataIA/sir-bone-rs/releases/latest/download/sir-bone-rs-installer.ps1 | iex"
```

### Con Rust (cargo)

```bash
cargo install sir-bone-rs --locked           # da crates.io
cargo install --git https://github.com/automataIA/sir-bone-rs --locked   # da sorgente
# oppure, nel checkout locale:
cargo install --path .                         # → ~/.cargo/bin/sirbone
```

> Target Windows supportato: **x86_64 (MSVC)**. Non ci sono build ufficiali per
> Windows ARM né Windows-GNU: su quelle macchine usa la via **Windows+WSL**.

### Trova il path assoluto (ti serve per la config di Zed)

```bash
which sirbone            # macOS / Linux / WSL   → es. /home/tu/.cargo/bin/sirbone
```
```powershell
(Get-Command sirbone).Source   # Windows → es. C:\Users\tu\.cargo\bin\sirbone.exe
```

Poi imposta la key una volta:

```bash
sirbone login            # scrive ~/.sirbone/.env (o %USERPROFILE%\.sirbone\.env)
```

---

## 2. Dove stanno le settings di Zed

Il modo più sicuro è la **command palette**: apri Zed → comando **`zed: open
settings`** → apre il file giusto per il tuo OS. Percorsi di riferimento:

| OS | File settings |
|---|---|
| macOS | `~/.config/zed/settings.json` |
| Linux | `~/.config/zed/settings.json` |
| Windows | `%APPDATA%\Zed\settings.json` |

In alternativa: `agent: open settings` → **External Agents** → **Add Custom
Agent** compila da solo la voce `agent_servers`.

---

## 3. macOS

Zed e `sirbone` girano entrambi su macOS. Aggiungi a `settings.json`:

```json
{
  "agent_servers": {
    "Sir Bone": {
      "type": "custom",
      "command": "/Users/tu/.cargo/bin/sirbone",
      "args": ["acp"],
      "env": { "SIRBONE_MODEL": "claude-opus-4-7" }
    }
  }
}
```

- `command`: path assoluto (da `which sirbone`).
- Se la key è già in `~/.sirbone/.env` (via `sirbone login`) puoi omettere `env`.
- Per passare la key esplicitamente: aggiungi `"ANTHROPIC_AUTH_TOKEN": "sk-..."`
  dentro `env`.

---

## 4. Linux

Identico a macOS, cambia solo il path del binario:

```json
{
  "agent_servers": {
    "Sir Bone": {
      "type": "custom",
      "command": "/home/tu/.cargo/bin/sirbone",
      "args": ["acp"],
      "env": { "SIRBONE_MODEL": "claude-opus-4-7" }
    }
  }
}
```

---

## 5. Windows (nativo)

Usa questa via **solo se apri progetti sul filesystem Windows** (es.
`C:\progetti\...`). Serve `sirbone.exe` (build MSVC).

```json
{
  "agent_servers": {
    "Sir Bone": {
      "type": "custom",
      "command": "C:\\Users\\tu\\.cargo\\bin\\sirbone.exe",
      "args": ["acp"],
      "env": { "SIRBONE_MODEL": "claude-opus-4-7" }
    }
  }
}
```

- Path assoluto con doppie backslash (`\\`) o slash (`/`) — entrambi validi in JSON.
- La config sta in `%USERPROFILE%\.sirbone\` (`sirbone login` in PowerShell la crea).
- **Non** puntare `sirbone.exe` a un progetto che vive dentro WSL (`\\wsl$\...`):
  i path non combaciano tra i due mondi. In quel caso usa la sezione seguente.

---

## 6. Windows + WSL (consigliato se sviluppi in WSL)

Zed gira su Windows, il codice sta in WSL (Ubuntu). La via pulita è **Zed remote
via SSH dentro WSL**: il server remoto di Zed gira *dentro* WSL e lancia lì
`sirbone`, così l'agente condivide filesystem e `cwd` Linux del progetto — path
nativi, **zero traduzione Windows↔Linux**.

### 6.1 — Prepara WSL (una volta)

Dentro la distro WSL:

```bash
# binario + key nell'ambiente Linux (NON in PowerShell)
cargo install --path .          # o l'installer .sh della sezione 1
sirbone login

# server SSH per il remote di Zed
sudo apt-get update && sudo apt-get install -y openssh-server
sudo service ssh start          # opzionale: abilitalo all'avvio
```

### 6.2 — Connetti Zed a WSL come progetto remoto

In Zed (Windows) apri il progetto WSL tramite il flusso **remote development /
SSH** di Zed, connettendoti alla tua distro (es. `tu@localhost` sulla porta SSH di
WSL, o l'hostname WSL). Riferimento: documentazione Zed su remote development.

### 6.3 — Config agente (lato WSL/remoto)

La voce `agent_servers` va nelle settings **usate sul lato remoto/WSL**, con path
**Linux**:

```json
{
  "agent_servers": {
    "Sir Bone": {
      "type": "custom",
      "command": "/home/tu/.cargo/bin/sirbone",
      "args": ["acp"],
      "env": {
        "ANTHROPIC_AUTH_TOKEN": "sk-...",
        "SIRBONE_MODEL": "claude-opus-4-7"
      }
    }
  }
}
```

> **Alternativa senza SSH (sconsigliata):** lanciare il binario Linux via
> `"command": "wsl.exe", "args": ["-d","Ubuntu","--","/home/tu/.cargo/bin/sirbone","acp"]`
> con Zed locale su Windows. Funziona per lo stdio, **ma** Zed invia `cwd` come
> path Windows (`\\wsl$\...`) che l'agente Linux non sa risolvere: le operazioni
> sui file falliscono. Sir Bone non traduce i path, quindi usa la via SSH.

---

## 7. Verifica che funzioni

1. Apri il **pannello Agent** → nuovo thread → scegli **Sir Bone** → invia un prompt.
2. Le tool call appaiono come card; quelle distruttive chiedono permesso.
3. Ispeziona il protocollo: command palette → **`dev: open acp logs`**.
4. Smoke da terminale (nell'ambiente dove gira l'agente, serve una key):
   ```bash
   bash playground/acp_smoke.sh
   # atteso: ok initialize / session/new / session/prompt / session/load → ACP SMOKE PASSED
   ```

---

## 8. Troubleshooting

| Sintomo | Causa probabile | Fix |
|---|---|---|
| L'agente non parte / thread muore subito | `command` non è un path assoluto valido, o manca la key | Verifica con `which sirbone` / `Get-Command`; `sirbone login` o metti la key in `env` |
| "no API key found" negli acp logs | Nessuna key né in `env` né in `~/.sirbone/.env` | Aggiungi `ANTHROPIC_AUTH_TOKEN` (o `OPENAI_API_KEY`) all'`env` della voce |
| I tool falliscono a leggere/scrivere file (WSL) | Agente lanciato dal lato sbagliato: gira su Windows ma il progetto è in WSL | Usa la via remote-SSH (sezione 6): l'agente deve girare **dentro** WSL |
| Nessun output ma il processo è vivo | stdout sporcato da altri messaggi | Sir Bone tiene stdout pulito per il JSON-RPC; controlla `dev: open acp logs` |
| Le immagini vengono ignorate | Endpoint non-Anthropic-vision | `promptCapabilities.image` è true solo sull'API Anthropic ufficiale |
| Voglio un altro modello | Default `claude-opus-4-7` | Imposta `SIRBONE_MODEL` nell'`env` della voce |

---

## Riferimenti

- Panoramica capacità/limiti: [`editors/zed/README.md`](./README.md)
- Protocollo: <https://agentclientprotocol.com/> · External agents in Zed:
  <https://zed.dev/docs/ai/external-agents>
- Smoke test: [`playground/acp_smoke.sh`](../../playground/acp_smoke.sh)

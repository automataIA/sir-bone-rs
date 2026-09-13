use std::{collections::BTreeMap, io::Write as _, path::Path, time::Duration};

use anyhow::{Context as _, Result};
use sirbone::{
    questions::{AnswerOrigin, Question, QuestionAnswer, QuestionOption, QuestionRound},
    verification_setup::{self, CandidateKind, Selection},
};

pub async fn run(cwd: &Path, json: bool) -> Result<()> {
    let discovery = verification_setup::discover(cwd);
    if json {
        println!("{}", serde_json::to_string_pretty(&discovery)?);
        return Ok(());
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("setup-verification richiede un TTY; nessuna configurazione è stata scritta. Usa --json per ispezionare i candidati.");
        return Ok(());
    }

    let authoritative: Vec<_> = discovery
        .candidates
        .iter()
        .filter(|c| c.kind == CandidateKind::Authoritative)
        .collect();
    let post_edit: Vec<_> = discovery
        .candidates
        .iter()
        .filter(|c| c.kind == CandidateKind::PostEdit)
        .collect();

    println!("Verifica deterministica per {}", cwd.display());
    let mut auth_options: Vec<QuestionOption> = authoritative
        .iter()
        .map(|c| {
            QuestionOption::new(
                &c.command,
                format!("eseguito dopo Done; rilevato da {}", c.source),
            )
        })
        .chain([QuestionOption::new(
            "Comando personalizzato",
            "inserisci manualmente il gate autorevole del progetto",
        )])
        .collect();
    if authoritative.is_empty() {
        auth_options.push(QuestionOption::new(
            "Annulla",
            "non viene scritta alcuna configurazione",
        ));
    }
    let auth_round = QuestionRound {
        questions: vec![Question {
            id: "test_command".into(),
            context: "Il comando scelto diventa il gate autorevole eseguito dopo Done; un fallimento può richiedere un altro tentativo.".into(),
            question: "Quale comando deve essere autorevole?".into(),
            options: auth_options,
        }],
    };
    let auth_index = ask_round(&auth_round)?[0].index.unwrap_or(0);
    if authoritative.is_empty() && auth_index == 1 {
        println!("Annullato; nessuna configurazione è stata scritta.");
        return Ok(());
    }
    let authoritative_command = if auth_index == authoritative.len() {
        read_nonempty("Comando autorevole: ")?
    } else {
        authoritative[auth_index].command.clone()
    };

    let mut post_groups: BTreeMap<&str, Vec<_>> = BTreeMap::new();
    for candidate in &post_edit {
        post_groups
            .entry(candidate.id.split('-').next().unwrap_or("other"))
            .or_default()
            .push(*candidate);
    }
    let post_round = QuestionRound {
        questions: if post_groups.is_empty() {
            vec![Question {
                id: "post_edit_manual".into(),
                context: "I controlli rapidi sono advisory e vengono eseguiti dopo modifiche ai file associati; non sostituiscono il gate autorevole.".into(),
                question: "Nessun controllo rapido affidabile rilevato. Configurarne uno?".into(),
                options: vec![
                    QuestionOption::new("Controllo personalizzato", "inserisci glob e comando da eseguire dopo le modifiche"),
                    QuestionOption::new("Nessun controllo rapido", "riceverai feedback solo dal gate autorevole dopo Done"),
                ],
            }]
        } else {
            post_groups
                .iter()
                .map(|(ecosystem, candidates)| Question {
                    id: format!("post_edit_{ecosystem}"),
                    context: format!("Il controllo {ecosystem} è advisory: scatta dopo modifiche ai glob indicati e può segnalare errori prima di Done."),
                    question: format!("Quale controllo rapido usare per {ecosystem}?"),
                    options: candidates
                        .iter()
                        .map(|c| {
                            QuestionOption::new(
                                &c.command,
                                format!(
                                    "eseguito per {}; rilevato da {}",
                                    c.globs.join(", "),
                                    c.source
                                ),
                            )
                        })
                        .chain([
                            QuestionOption::new("Controllo personalizzato", "inserisci manualmente glob e comando post-edit"),
                            QuestionOption::new("Nessun controllo rapido", "non eseguire controlli advisory per questo ecosistema"),
                        ])
                        .collect(),
                })
                .collect()
        },
    };
    let mut post = BTreeMap::new();
    let post_answers = ask_round(&post_round)?;
    if post_groups.is_empty() {
        if post_answers[0].index == Some(0) {
            add_manual_post(&mut post, read_nonempty("Comando rapido: ")?)?;
        }
    } else {
        for ((_, candidates), answer) in post_groups.iter().zip(post_answers) {
            let index = answer.index.unwrap_or(candidates.len() + 1);
            if let Some(candidate) = candidates.get(index) {
                for glob in &candidate.globs {
                    post.insert(glob.clone(), candidate.command.clone());
                }
            } else if index == candidates.len() {
                add_manual_post(&mut post, read_nonempty("Comando rapido: ")?)?;
            }
        }
    }

    let risk_round = QuestionRound {
        questions: vec![Question {
            id: "high_risk".into(),
            context: "Il preset usa il normale prompt Allow once/always/Deny per dipendenze, migrazioni, schemi e modifiche riconoscibili alle API pubbliche. Le operazioni ordinarie restano silenziose.".into(),
            question: "Abilitare la conferma per operazioni ad alto rischio?".into(),
            options: vec![
                QuestionOption::new("Abilita high_risk", "aggiunge hooks.presets: [\"high_risk\"] alla modifica mostrata prima del salvataggio"),
                QuestionOption::new("Lascia disabilitato", "non aggiunge alcun preset e conserva il comportamento attuale"),
            ],
        }],
    };
    let high_risk = ask_round(&risk_round)?[0].index == Some(0);

    let mut selection = Selection {
        authoritative: authoritative_command,
        post_edit: post,
        max_attempts: 3,
        high_risk,
    };
    loop {
        println!("\nDirectory: {}", cwd.display());
        println!(
            "Modifica esatta:\n{}",
            serde_json::to_string_pretty(&verification_setup::config_patch(&selection))?
        );
        let final_round = QuestionRound {
            questions: vec![Question {
                id: "confirm".into(),
                context: "La modifica mostrata sopra verrà applicata atomicamente alla configurazione del progetto, preservando le altre chiavi.".into(),
                question: "Come procedere?".into(),
                options: vec![
                    QuestionOption::new("Esegui e salva", "prova ora il comando autorevole e salva solo dopo il risultato"),
                    QuestionOption::new("Salva senza eseguire", "scrive la configurazione senza verificarne ora il comando"),
                    QuestionOption::new("Modifica", "cambia manualmente comando autorevole e controlli rapidi"),
                    QuestionOption::new("Annulla", "non scrive alcuna modifica"),
                ],
            }],
        };
        match ask_round(&final_round)?[0].index.unwrap_or(3) {
            0 => {
                let ok = run_command(cwd, &selection.authoritative).await?;
                if !ok {
                    let failed = QuestionRound {
                        questions: vec![Question {
                            id: "save_failed".into(),
                            context: "Il comando autorevole appena eseguito è fallito. Salvandolo, le esecuzioni future potranno essere bloccate finché il progetto non torna verde.".into(),
                            question: "Il comando è fallito. Salvare comunque?".into(),
                            options: vec![
                                QuestionOption::new("Non salvare", "torna alla configurazione senza registrare il comando fallito"),
                                QuestionOption::new("Salva comunque", "registra consapevolmente un gate attualmente rosso"),
                            ],
                        }],
                    };
                    if ask_round(&failed)?[0].index != Some(1) {
                        continue;
                    }
                }
                let path = verification_setup::save(cwd, &selection)?;
                println!("Configurazione salvata atomicamente in {}", path.display());
                return Ok(());
            }
            1 => {
                let path = verification_setup::save(cwd, &selection)?;
                println!("Configurazione salvata atomicamente in {}", path.display());
                return Ok(());
            }
            2 => {
                selection.authoritative = read_nonempty("Comando autorevole: ")?;
                let globs =
                    read_nonempty("Glob rapidi (vuoto non consentito; usa '-' per nessuno): ")?;
                selection.post_edit.clear();
                if globs != "-" {
                    let command = read_nonempty("Comando rapido: ")?;
                    for glob in globs.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        selection.post_edit.insert(glob.into(), command.clone());
                    }
                }
            }
            _ => {
                println!("Annullato; nessuna configurazione è stata scritta.");
                return Ok(());
            }
        }
    }
}

fn ask_round(round: &QuestionRound) -> Result<Vec<QuestionAnswer>> {
    round.validate().map_err(anyhow::Error::msg)?;
    let mut answers = Vec::with_capacity(round.questions.len());
    for q in &round.questions {
        println!("\n{}", q.question);
        if !q.context.trim().is_empty() {
            println!("  {}", q.context);
        }
        for (i, option) in q.options.iter().enumerate() {
            println!(
                "  {}. {}{}",
                i + 1,
                option.display(),
                if i == 0 { " (raccomandato)" } else { "" }
            );
        }
        loop {
            let raw = read_nonempty("Scelta: ")?;
            if let Ok(index) = raw.parse::<usize>() {
                if (1..=q.options.len()).contains(&index) {
                    let index = index - 1;
                    answers.push(QuestionAnswer {
                        id: q.id.clone(),
                        value: q.options[index].label.clone(),
                        index: Some(index),
                        origin: AnswerOrigin::User,
                    });
                    break;
                }
            }
            eprintln!("Scelta non valida.");
        }
    }
    Ok(answers)
}

fn add_manual_post(post: &mut BTreeMap<String, String>, command: String) -> Result<()> {
    let globs = read_nonempty("Glob (separati da virgola): ")?;
    for glob in globs.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        post.insert(glob.into(), command.clone());
    }
    Ok(())
}

fn read_nonempty(prompt: &str) -> Result<String> {
    loop {
        print!("{prompt}");
        std::io::stdout().flush()?;
        let mut value = String::new();
        std::io::stdin().read_line(&mut value)?;
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Ok(value);
        }
    }
}

async fn run_command(cwd: &Path, command: &str) -> Result<bool> {
    println!("Eseguo in {}: {}", cwd.display(), command);
    #[cfg(windows)]
    let mut child = {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut child = {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    child.current_dir(cwd);
    let output = match tokio::time::timeout(Duration::from_secs(300), child.output()).await {
        Ok(output) => output.context("impossibile avviare il comando di verifica")?,
        Err(_) => {
            eprintln!("timeout: il comando non ha terminato entro 300s");
            return Ok(false);
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{}{}", truncate(&stdout), truncate(&stderr));
    println!(
        "exit: {}",
        output
            .status
            .code()
            .map_or_else(|| "signal".into(), |c| c.to_string())
    );
    Ok(output.status.success())
}

fn truncate(value: &str) -> &str {
    const LIMIT: usize = 16_000;
    if value.len() <= LIMIT {
        return value;
    }
    let mut end = LIMIT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

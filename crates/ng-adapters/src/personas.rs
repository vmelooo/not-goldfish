//! Universal personas: harness-agnostic role definitions (CEO, PM, dev, ...)
//! that get synced out into each harness's own subagent/persona format.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{atomic_write, backup_if_exists, Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Persona {
    pub name: String,
    pub description: String,
    pub body_md: String,
}

/// Three starter personas, useful out of the box rather than placeholders:
/// CEO (product vision/prioritization), PM (scope/requirements/acceptance),
/// dev (implementation/tests). All instruct the persona to lean on
/// `ng search` for continuity across sessions — that's the whole point of
/// giving a harness-agnostic persona a memory layer under it.
pub fn default_personas() -> Vec<Persona> {
    vec![
        Persona {
            name: "ceo".to_string(),
            description: "Visão de produto, priorização e trade-offs de negócio".to_string(),
            body_md: "\
Você atua como CEO/dono de produto nesta sessão. Seu trabalho é decidir o \
que importa agora, não como implementar.

Responsabilidades:
- Priorizar: dado um conjunto de pedidos/ideias, diga o que entra primeiro e por quê.
- Trade-offs: torne explícito o que se ganha e o que se sacrifica em cada decisão.
- Visão: mantenha o norte do produto; sinalize quando um pedido foge do escopo.
- Risco: aponte riscos de negócio (não só técnicos) antes de comprometer recursos.

Antes de decidir, rode `ng search <tema>` para recuperar decisões e \
contexto de sessões anteriores — não repita uma decisão já tomada, e não \
contradiga uma sem justificar a mudança.

Seja direto. Prefira uma recomendação clara com a razão por trás a uma \
lista de opções sem posição.\n"
                .to_string(),
        },
        Persona {
            name: "pm".to_string(),
            description: "Escopo, requisitos e critérios de aceite de features".to_string(),
            body_md: "\
Você atua como PM nesta sessão. Seu trabalho é transformar uma intenção \
vaga em algo implementável e verificável.

Responsabilidades:
- Escopo: defina o que está dentro e o que está fora explicitamente.
- Requisitos: liste requisitos funcionais e não-funcionais relevantes.
- Critérios de aceite: escreva critérios testáveis (dado/quando/então ou \
  uma lista de condições verificáveis), não frases vagas como \"funciona bem\".
- Edge cases: pergunte sobre o que acontece nos casos limite antes de \
  considerar o requisito fechado.

Use `ng search <feature ou área>` para achar decisões de escopo já \
tomadas em sessões anteriores antes de definir requisitos novos — evita \
retrabalho e contradição com o que já foi combinado.

Entregue sempre um artefato objetivo: escopo + requisitos + critérios de \
aceite, pronto para ir para implementação sem mais perguntas óbvias.\n"
                .to_string(),
        },
        Persona {
            name: "dev".to_string(),
            description: "Implementação, testes e qualidade de código".to_string(),
            body_md: "\
Você atua como desenvolvedor(a) nesta sessão. Seu trabalho é implementar \
com qualidade, não só fazer funcionar.

Responsabilidades:
- Implementação mínima e correta: resolva o problema pedido, sem escopo extra.
- Testes: todo comportamento novo ou corrigido vem com teste que falha \
  antes da mudança e passa depois.
- Legibilidade: código lido por outra pessoa (ou por você em 3 meses) sem \
  precisar perguntar \"por quê\".
- Verificação: rode build e testes antes de declarar algo pronto; nunca \
  afirme que algo funciona sem ter rodado.

Antes de implementar algo que parece já ter sido discutido, rode \
`ng search <termo>` para recuperar contexto de decisões técnicas e bugs \
anteriores relacionados — evita refazer uma investigação já feita.

Reporte o que foi feito com evidência (comando rodado, resultado), não \
com afirmação sem prova.\n"
                .to_string(),
        },
    ]
}

/// Read every `*.md` file directly under `dir` as a persona. Frontmatter is
/// a minimal hand-rolled format (no YAML dependency): the file must start
/// with a `---` line, then `name: ...` / `description: ...` lines (any
/// order, either may be missing), then a closing `---` line; everything
/// after that closing line is `body_md`. A file with no frontmatter is
/// skipped rather than erroring — this reads a directory of *personas*,
/// not arbitrary markdown, so it tolerates stray files silently.
pub fn load_personas(dir: &Path) -> Vec<Persona> {
    let mut personas = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return personas;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(persona) = parse_frontmatter(&raw, &path) {
            personas.push(persona);
        }
    }
    personas.sort_by(|a, b| a.name.cmp(&b.name));
    personas
}

fn parse_frontmatter(raw: &str, path: &Path) -> Option<Persona> {
    let mut lines = raw.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut name = None;
    let mut description = None;
    let mut consumed = 1; // the opening "---"
    for line in lines.by_ref() {
        consumed += 1;
        let trimmed = line.trim();
        if trimmed == "---" {
            let body_md = raw
                .lines()
                .skip(consumed)
                .collect::<Vec<_>>()
                .join("\n")
                .trim_start_matches('\n')
                .to_string();
            let default_name = || path.file_stem().map(|s| s.to_string_lossy().to_string());
            return Some(Persona {
                name: name.or_else(default_name)?,
                description: description.unwrap_or_default(),
                body_md,
            });
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            name = Some(value.trim().to_string());
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            description = Some(value.trim().to_string());
        }
    }
    None // no closing "---" found: not a valid persona file
}

/// Write each persona as `<agents_dir>/<name>.md` in Claude Code's
/// subagent format (YAML-ish frontmatter with `name`/`description`,
/// followed by the body). Overwrites any file with the same name —
/// re-syncing personas is expected to be idempotent, not additive.
pub fn sync_claude(personas: &[Persona], agents_dir: &Path) -> Result<()> {
    for persona in personas {
        let path = agents_dir.join(format!("{}.md", persona.name));
        let content = format!(
            "---\nname: {}\ndescription: {}\n---\n{}",
            persona.name, persona.description, persona.body_md
        );
        atomic_write(&path, content.as_bytes())?;
    }
    Ok(())
}

/// Merge each persona into opencode's `"agent"` config key:
/// `{name: {description, prompt}}`, preserving every other top-level key
/// and every agent not in `personas`.
pub fn sync_opencode(personas: &[Persona], config_path: &Path) -> Result<Option<PathBuf>> {
    let mut config: Value = if config_path.exists() {
        let raw = std::fs::read_to_string(config_path)?;
        serde_json::from_str(&raw).map_err(|e| {
            Error::Other(format!("{} não é JSON válido: {e}", config_path.display()))
        })?
    } else {
        json!({})
    };
    let backup = backup_if_exists(config_path)?;

    let agents = config
        .as_object_mut()
        .ok_or_else(|| Error::Other(format!("{} não é um objeto JSON", config_path.display())))?
        .entry("agent")
        .or_insert_with(|| json!({}));
    let agents_obj = agents
        .as_object_mut()
        .ok_or_else(|| Error::Other("\"agent\" não é um objeto".to_string()))?;

    for persona in personas {
        agents_obj.insert(
            persona.name.clone(),
            json!({ "description": persona.description, "prompt": persona.body_md }),
        );
    }

    atomic_write(
        config_path,
        serde_json::to_string_pretty(&config)?.as_bytes(),
    )?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_personas_have_the_three_expected_roles() {
        let personas = default_personas();
        let names: Vec<&str> = personas.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["ceo", "pm", "dev"]);
        for p in &personas {
            assert!(!p.description.is_empty());
            assert!(
                p.body_md.contains("ng search"),
                "{} mentions ng search",
                p.name
            );
            assert!(p.body_md.lines().count() >= 8);
        }
    }

    #[test]
    fn claude_sync_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let personas = default_personas();
        sync_claude(&personas, &agents_dir).unwrap();

        let loaded = load_personas(&agents_dir);
        assert_eq!(loaded.len(), 3);
        for original in &personas {
            let found = loaded.iter().find(|p| p.name == original.name).unwrap();
            assert_eq!(found.description, original.description);
            assert_eq!(found.body_md.trim_end(), original.body_md.trim_end());
        }
    }

    #[test]
    fn load_personas_skips_files_without_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("README.md"), "# not a persona\njust notes").unwrap();
        std::fs::write(
            tmp.path().join("real.md"),
            "---\nname: real\ndescription: a real one\n---\nbody here\n",
        )
        .unwrap();

        let loaded = load_personas(tmp.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "real");
        assert_eq!(loaded[0].body_md.trim_end(), "body here");
    }

    #[test]
    fn load_personas_on_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_personas(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn opencode_sync_merges_and_preserves_other_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("opencode.json");
        std::fs::write(
            &config_path,
            r#"{"theme":"dark","agent":{"custom":{"description":"d","prompt":"p"}}}"#,
        )
        .unwrap();

        let personas = default_personas();
        let backup = sync_opencode(&personas, &config_path).unwrap();
        assert!(backup.is_some());

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["agent"]["custom"]["description"], "d");
        assert_eq!(
            value["agent"]["ceo"]["description"],
            default_personas()[0].description
        );
        assert!(value["agent"]["dev"]["prompt"]
            .as_str()
            .unwrap()
            .contains("ng search"));
    }
}

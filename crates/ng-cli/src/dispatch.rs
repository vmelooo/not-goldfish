//! `ng dispatch`: suggests a model tier for a prompt via
//! `ng_adapters::dispatch`, optionally seeded from a user-editable
//! `~/.not-goldfish/dispatch.toml`.

use std::path::Path;

use anyhow::Context;
use ng_adapters::dispatch::{suggest, DispatchConfig};

use crate::ui::Palette;

/// Commented starter file, written by `--init`. Kept as a literal template
/// (rather than generated from [`DispatchConfig::default_config`]) so it
/// can carry explanatory comments; [`tests::default_dispatch_toml_matches_builtin_rules`]
/// guards it from drifting out of sync with the real defaults.
const DEFAULT_DISPATCH_TOML: &str = r#"# not-goldfish dispatch.toml — só é preciso declarar o que você quer
# mudar; qualquer categoria/keyword ausente cai no embutido do not-goldfish.
# Categorias fixas: quick_fix, standard, architecture, research.

[rules]
quick_fix = "haiku"
standard = "sonnet"
architecture = "opus"
research = "opus"

[keywords]
quick_fix = ["fix", "bug", "typo", "corrigir", "conserta", "ajustar", "rápido"]
standard = ["implement", "implementar", "add", "adicionar", "feature", "refactor", "refatorar"]
architecture = ["architecture", "arquitetura", "design", "migrate", "migração", "sistema", "escalar"]
research = ["research", "pesquisar", "investigate", "investigar", "analyze", "analisar", "comparar"]
"#;

pub fn dispatch(prompt: &str, init: bool) -> anyhow::Result<()> {
    let config_path = ng_core::paths::data_dir().join("dispatch.toml");

    if init {
        write_default_dispatch_toml(&config_path)?;
        let p = Palette::detect();
        println!(
            "dispatch.toml padrão escrito em {} — {}",
            p.bold(config_path.display()),
            p.dim("edite [rules]/[keywords] para customizar")
        );
        return Ok(());
    }

    if prompt.trim().is_empty() {
        anyhow::bail!("informe um prompt para classificar, ou rode `ng dispatch --init` para gerar o dispatch.toml");
    }

    let config = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("lendo {}", config_path.display()))?;
        DispatchConfig::from_toml(&raw).map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        DispatchConfig::default_config()
    };

    let suggestion = suggest(&config, prompt);
    let p = Palette::detect();
    println!("{}", p.kv("categoria", p.teal(&suggestion.category)));
    println!("{}", p.kv("modelo sugerido", p.gold(&suggestion.model)));
    if suggestion.matched.is_empty() {
        println!(
            "{}",
            p.kv("keywords", p.muted("(nenhuma casou — fallback padrão)"))
        );
    } else {
        println!("{}", p.kv("keywords", suggestion.matched.join(", ")));
    }
    Ok(())
}

fn write_default_dispatch_toml(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_DISPATCH_TOML)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dispatch_toml_matches_builtin_rules() {
        let config = DispatchConfig::from_toml(DEFAULT_DISPATCH_TOML).unwrap();
        let builtin = DispatchConfig::default_config();
        assert_eq!(
            config.rules, builtin.rules,
            "template rules.toml drifted from DispatchConfig::default_config()"
        );
    }

    #[test]
    fn default_dispatch_toml_still_classifies_as_expected() {
        let config = DispatchConfig::from_toml(DEFAULT_DISPATCH_TOML).unwrap();
        let s = suggest(&config, "corrigir esse bug rápido");
        assert_eq!(s.category, "quick_fix");
        assert_eq!(s.model, "haiku");
    }
}

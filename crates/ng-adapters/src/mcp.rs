//! MCP server registration for Claude Code (`settings.json`, JSON) and
//! Codex (`config.toml`, TOML). Both merge into an existing document
//! instead of overwriting it, and both back it up first.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{atomic_write, backup_if_exists, Error, Result};

/// Register (or replace) an MCP server entry under `mcpServers` in a Claude
/// Code-style `settings.json`. Existing entries for *other* servers, and
/// every other top-level key, are left untouched.
pub fn register_mcp_claude(
    settings_path: &Path,
    name: &str,
    command: &str,
    args: &[&str],
) -> Result<Option<PathBuf>> {
    let mut settings: Value = if settings_path.exists() {
        let raw = std::fs::read_to_string(settings_path)?;
        serde_json::from_str(&raw).map_err(|e| {
            Error::Other(format!(
                "{} não é JSON válido: {e}",
                settings_path.display()
            ))
        })?
    } else {
        json!({})
    };
    let backup = backup_if_exists(settings_path)?;

    let servers = settings
        .as_object_mut()
        .ok_or_else(|| Error::Other(format!("{} não é um objeto JSON", settings_path.display())))?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let servers_obj = servers
        .as_object_mut()
        .ok_or_else(|| Error::Other("\"mcpServers\" não é um objeto".to_string()))?;

    servers_obj.insert(
        name.to_string(),
        json!({ "command": command, "args": args }),
    );

    atomic_write(
        settings_path,
        serde_json::to_string_pretty(&settings)?.as_bytes(),
    )?;
    Ok(backup)
}

/// Register (or replace) an MCP server entry under `[mcp_servers.<name>]`
/// in a Codex-style `config.toml`, preserving every other table/key in the
/// document (`toml::Value`, never a rigid struct — Codex's config schema
/// is not ours to pin down).
pub fn register_mcp_codex(
    config_toml_path: &Path,
    name: &str,
    command: &str,
    args: &[&str],
) -> Result<Option<PathBuf>> {
    let mut doc: toml::Value = if config_toml_path.exists() {
        let raw = std::fs::read_to_string(config_toml_path)?;
        toml::from_str(&raw)?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let backup = backup_if_exists(config_toml_path)?;

    let root = doc.as_table_mut().ok_or_else(|| {
        Error::Other(format!(
            "{} não é uma tabela TOML",
            config_toml_path.display()
        ))
    })?;
    let servers = root
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let servers_table = servers
        .as_table_mut()
        .ok_or_else(|| Error::Other("\"mcp_servers\" não é uma tabela TOML".to_string()))?;

    let mut entry = toml::map::Map::new();
    entry.insert(
        "command".to_string(),
        toml::Value::String(command.to_string()),
    );
    entry.insert(
        "args".to_string(),
        toml::Value::Array(
            args.iter()
                .map(|a| toml::Value::String(a.to_string()))
                .collect(),
        ),
    );
    servers_table.insert(name.to_string(), toml::Value::Table(entry));

    atomic_write(config_toml_path, toml::to_string_pretty(&doc)?.as_bytes())?;
    Ok(backup)
}

/// Standard MCP entry for the `browser-use` server (Playwright-driven
/// browser automation over MCP), shared by every adapter that wants to
/// offer it: `(name, command, args)`.
pub fn browser_use_entry() -> (&'static str, &'static str, Vec<&'static str>) {
    (
        "browser-use",
        "uvx",
        vec!["--from", "browser-use[cli]", "browser-use", "--mcp"],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_register_merges_into_existing_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"theme":"dark","mcpServers":{"other":{"command":"foo","args":[]}}}"#,
        )
        .unwrap();

        let (name, command, args) = browser_use_entry();
        let backup = register_mcp_claude(&path, name, command, &args).unwrap();
        assert!(backup.is_some());

        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["mcpServers"]["other"]["command"], "foo");
        assert_eq!(value["mcpServers"]["browser-use"]["command"], "uvx");
        assert_eq!(
            value["mcpServers"]["browser-use"]["args"],
            json!(["--from", "browser-use[cli]", "browser-use", "--mcp"])
        );
    }

    #[test]
    fn claude_register_on_fresh_file_has_no_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/settings.json");
        let backup = register_mcp_claude(&path, "s", "cmd", &["a"]).unwrap();
        assert!(backup.is_none());
        assert!(path.exists());
    }

    #[test]
    fn codex_register_preserves_other_toml_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "model = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"foo\"\nargs = []\n",
        )
        .unwrap();

        let (name, command, args) = browser_use_entry();
        let backup = register_mcp_codex(&path, name, command, &args).unwrap();
        assert!(backup.is_some());

        let doc: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        assert_eq!(doc["mcp_servers"]["other"]["command"].as_str(), Some("foo"));
        assert_eq!(
            doc["mcp_servers"]["browser-use"]["command"].as_str(),
            Some("uvx")
        );
        let args_toml = doc["mcp_servers"]["browser-use"]["args"]
            .as_array()
            .unwrap();
        assert_eq!(args_toml.len(), 4);
    }

    #[test]
    fn codex_register_on_missing_file_creates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/config.toml");
        let backup = register_mcp_codex(&path, "browser-use", "uvx", &["--mcp"]).unwrap();
        assert!(backup.is_none());
        let doc: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            doc["mcp_servers"]["browser-use"]["command"].as_str(),
            Some("uvx")
        );
    }

    #[test]
    fn malformed_toml_errors_instead_of_clobbering() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "not = valid = toml = at = all").unwrap();
        let err = register_mcp_codex(&path, "s", "cmd", &["a"]);
        assert!(err.is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "not = valid = toml = at = all"
        );
    }
}

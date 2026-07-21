//! Hook installers: register `ng-hook` (or any command) as a harness hook.
//!
//! Both harnesses covered here use the same JSON shape at the top level —
//! `{"hooks": {<event>: [{"hooks": [{"type": "command", "command": ...}]}]}}`
//! — which is why one merge routine backs both `install_claude_style` and
//! `install_gemini`. What differs is only the event *names* each harness
//! emits, tracked in `DEFAULT_CLAUDE_EVENTS` / `DEFAULT_GEMINI_EVENTS`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{atomic_write, backup_if_exists, Error, Result};

/// Claude Code (and Kimi Code, which clones the same hooks schema) events
/// not-goldfish captures against by default.
pub const DEFAULT_CLAUDE_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PostToolUse",
    "SessionStart",
    "Stop",
    "PreCompact",
];

/// Gemini CLI's hook event names are **not** verified against upstream
/// docs at the time of writing — Gemini's hooks API is newer and less
/// documented than Claude Code's. These are our best-guess equivalents,
/// meant to be corrected once confirmed against a real Gemini CLI install:
/// `BeforeAgent` ≈ `UserPromptSubmit` (start of a turn), `AfterTool` ≈
/// `PostToolUse`, `SessionStart`/`SessionEnd` map directly by name.
pub const DEFAULT_GEMINI_EVENTS: &[&str] =
    &["BeforeAgent", "AfterTool", "SessionStart", "SessionEnd"];

/// Register `hook_bin` as a `"type": "command"` hook for each of `events`
/// in a Claude Code-style `settings.json` at `settings_path`, preserving
/// every other key already in the file (and every hook already registered
/// for a *different* command). Idempotent: running it again with the same
/// `hook_bin` does not duplicate entries.
///
/// Returns the backup path written before the swap, `None` if
/// `settings_path` didn't exist yet.
pub fn install_claude_style(
    settings_path: &Path,
    hook_bin: &str,
    events: &[&str],
) -> Result<Option<PathBuf>> {
    merge_command_hooks(settings_path, hook_bin, events)
}

/// Register `hook_bin` against [`DEFAULT_GEMINI_EVENTS`] in a Gemini
/// CLI-style settings file. See the module docs and the caveat on
/// [`DEFAULT_GEMINI_EVENTS`] about schema uncertainty.
pub fn install_gemini(settings_path: &Path, hook_bin: &str) -> Result<Option<PathBuf>> {
    merge_command_hooks(settings_path, hook_bin, DEFAULT_GEMINI_EVENTS)
}

fn merge_command_hooks(
    settings_path: &Path,
    hook_bin: &str,
    events: &[&str],
) -> Result<Option<PathBuf>> {
    let mut settings = load_or_init_json(settings_path)?;
    let backup = backup_if_exists(settings_path)?;

    let hooks = settings
        .as_object_mut()
        .ok_or_else(|| Error::Other(format!("{} não é um objeto JSON", settings_path.display())))?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| Error::Other("\"hooks\" não é um objeto".to_string()))?;

    for event in events {
        let entry = json!({ "hooks": [{ "type": "command", "command": hook_bin }] });
        let list = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| json!([]));
        let arr = list
            .as_array_mut()
            .ok_or_else(|| Error::Other(format!("entrada de hook \"{event}\" não é um array")))?;
        let already_registered = arr.iter().any(|matcher| {
            matcher
                .pointer("/hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|commands| {
                    commands.iter().any(|c| {
                        c.get("command")
                            .and_then(|v| v.as_str())
                            .is_some_and(|c| c == hook_bin)
                    })
                })
        });
        if !already_registered {
            arr.push(entry);
        }
    }

    atomic_write(
        settings_path,
        serde_json::to_string_pretty(&settings)?.as_bytes(),
    )?;
    Ok(backup)
}

/// Result of [`uninstall_ng_hooks`]: either nothing of ours was present
/// (no write happened at all), or entries were removed — with the backup
/// written before the swap and the list of events touched.
#[derive(Debug)]
pub enum UninstallOutcome {
    /// Settings file missing, or present but with no ng-hook entries.
    /// The file was not modified and no backup was taken.
    NotInstalled,
    /// ng-hook entries were removed from `events`; `backup` is the
    /// pre-removal copy of the settings file.
    Removed {
        backup: Option<PathBuf>,
        events: Vec<String>,
    },
}

/// Remove every hook entry whose command invokes an `ng-hook` binary from
/// a Claude Code-style (or Gemini, same shape) `settings.json` — the exact
/// inverse of [`install_claude_style`] / [`install_gemini`].
///
/// "Ours" is decided by [`is_ng_hook_command`]: any whitespace-separated
/// token of the command string whose file name is exactly `ng-hook` (with
/// or without the `env NG_HARNESS=... '<path>'` wrapper install writes,
/// and regardless of where the binary lived at install time). Every other
/// key, event, and hook entry is preserved byte-for-byte in JSON terms.
/// Event arrays left empty by the removal are dropped (install created
/// them); the `"hooks"` object itself is kept even if empty, since we
/// can't know whether install created it or the user did.
///
/// No-op safety: when nothing of ours is found the file is not rewritten
/// and no backup is taken. When something is removed the write goes
/// through the same backup + `atomic_write` path as install — the file is
/// never left truncated.
pub fn uninstall_ng_hooks(settings_path: &Path) -> Result<UninstallOutcome> {
    if !settings_path.exists() {
        return Ok(UninstallOutcome::NotInstalled);
    }
    let mut settings = load_or_init_json(settings_path)?;

    let Some(hooks_obj) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(UninstallOutcome::NotInstalled);
    };

    let mut touched_events = Vec::new();
    for (event, list) in hooks_obj.iter_mut() {
        let Some(arr) = list.as_array_mut() else {
            continue;
        };
        let mut removed_here = false;
        arr.retain_mut(|matcher| {
            let Some(commands) = matcher.pointer_mut("/hooks").and_then(|h| h.as_array_mut())
            else {
                return true; // not the shape we write — never ours, keep
            };
            let before = commands.len();
            commands.retain(|c| {
                !c.get("command")
                    .and_then(|v| v.as_str())
                    .is_some_and(is_ng_hook_command)
            });
            if commands.len() != before {
                removed_here = true;
            }
            // Drop the matcher entry only when *we* emptied it; an entry
            // that was already empty (user-authored oddity) is kept as-is.
            !(commands.is_empty() && before > 0)
        });
        if removed_here {
            touched_events.push(event.clone());
        }
    }
    if touched_events.is_empty() {
        return Ok(UninstallOutcome::NotInstalled);
    }

    // Drop event keys whose arrays we emptied — install created them.
    hooks_obj.retain(|event, list| {
        !(touched_events.contains(event) && list.as_array().is_some_and(|a| a.is_empty()))
    });

    let backup = backup_if_exists(settings_path)?;
    atomic_write(
        settings_path,
        serde_json::to_string_pretty(&settings)?.as_bytes(),
    )?;
    Ok(UninstallOutcome::Removed {
        backup,
        events: touched_events,
    })
}

/// True when `command` invokes an `ng-hook` binary: some whitespace-
/// separated token, after stripping the POSIX single quotes install may
/// have wrapped it in, has `ng-hook` as its exact file name. Matches the
/// plain-path form (Claude) and the `env NG_HARNESS=kimi '<path>'` form
/// (Kimi/Gemini) alike, without matching e.g. `/notes/ng-hook.txt` or a
/// different tool that merely mentions ng-hook in an argument name.
fn is_ng_hook_command(command: &str) -> bool {
    command.split_whitespace().any(|token| {
        let token = token.trim_matches('\'');
        Path::new(token)
            .file_name()
            .is_some_and(|name| name == "ng-hook")
    })
}

fn load_or_init_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(path)?;
    // Never guess at a file we can't parse — surface a clear error
    // instead of clobbering something the user hand-edited.
    serde_json::from_str(&raw)
        .map_err(|e| Error::Other(format!("{} não é JSON válido: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_on_fresh_file_creates_dirs_and_all_events() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join(".claude/settings.json");
        let backup = install_claude_style(
            &settings_path,
            "/usr/local/bin/ng-hook",
            DEFAULT_CLAUDE_EVENTS,
        )
        .unwrap();
        assert!(backup.is_none(), "no prior file, nothing to back up");

        let raw = std::fs::read_to_string(&settings_path).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        for event in DEFAULT_CLAUDE_EVENTS {
            let commands = value["hooks"][event][0]["hooks"].as_array().unwrap();
            assert_eq!(commands[0]["command"], "/usr/local/bin/ng-hook");
        }
    }

    #[test]
    fn install_preserves_existing_unrelated_keys_and_backs_up() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"theme":"dark","hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"/other/tool"}]}]}}"#,
        )
        .unwrap();

        let backup = install_claude_style(
            &settings_path,
            "/usr/local/bin/ng-hook",
            DEFAULT_CLAUDE_EVENTS,
        )
        .unwrap()
        .expect("file existed, must be backed up");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            r#"{"theme":"dark","hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"/other/tool"}]}]}}"#
        );

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark", "unrelated top-level key preserved");
        let post_tool_use = value["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            post_tool_use.len(),
            2,
            "other tool's hook kept, ours appended"
        );
        assert_eq!(post_tool_use[0]["hooks"][0]["command"], "/other/tool");
        assert_eq!(
            post_tool_use[1]["hooks"][0]["command"],
            "/usr/local/bin/ng-hook"
        );
    }

    #[test]
    fn install_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        install_claude_style(&settings_path, "/bin/ng-hook", &["UserPromptSubmit"]).unwrap();
        install_claude_style(&settings_path, "/bin/ng-hook", &["UserPromptSubmit"]).unwrap();

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            value["hooks"]["UserPromptSubmit"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn malformed_existing_file_errors_instead_of_clobbering() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(&settings_path, "not json at all").unwrap();
        let err = install_claude_style(&settings_path, "/bin/ng-hook", DEFAULT_CLAUDE_EVENTS);
        assert!(err.is_err());
        assert_eq!(
            std::fs::read_to_string(&settings_path).unwrap(),
            "not json at all"
        );
    }

    #[test]
    fn uninstall_removes_only_our_entries_and_preserves_others() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(
            &settings_path,
            r#"{"theme":"dark","hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"/other/tool"}]}]}}"#,
        )
        .unwrap();
        install_claude_style(
            &settings_path,
            "/usr/local/bin/ng-hook",
            DEFAULT_CLAUDE_EVENTS,
        )
        .unwrap();

        let outcome = uninstall_ng_hooks(&settings_path).unwrap();
        let UninstallOutcome::Removed { backup, mut events } = outcome else {
            panic!("expected Removed");
        };
        assert!(backup.is_some(), "file existed, must be backed up");
        events.sort();
        let mut expected: Vec<String> = DEFAULT_CLAUDE_EVENTS
            .iter()
            .map(|e| e.to_string())
            .collect();
        expected.sort();
        assert_eq!(events, expected);

        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(value["theme"], "dark", "unrelated top-level key preserved");
        let post_tool_use = value["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post_tool_use.len(), 1, "other tool's hook survives");
        assert_eq!(post_tool_use[0]["hooks"][0]["command"], "/other/tool");
        for event in DEFAULT_CLAUDE_EVENTS
            .iter()
            .filter(|e| **e != "PostToolUse")
        {
            assert!(
                value["hooks"].get(*event).is_none(),
                "event {event} we created and emptied must be dropped"
            );
        }
    }

    #[test]
    fn uninstall_removes_env_wrapped_kimi_style_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        install_claude_style(
            &settings_path,
            "env NG_HARNESS=kimi '/opt/not goldfish/ng-hook'",
            &["UserPromptSubmit"],
        )
        .unwrap();

        let outcome = uninstall_ng_hooks(&settings_path).unwrap();
        assert!(matches!(outcome, UninstallOutcome::Removed { .. }));
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(value["hooks"].get("UserPromptSubmit").is_none());
    }

    #[test]
    fn uninstall_is_a_no_op_when_nothing_of_ours_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let original = r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"/other/tool"}]}]}}"#;
        std::fs::write(&settings_path, original).unwrap();

        let outcome = uninstall_ng_hooks(&settings_path).unwrap();
        assert!(matches!(outcome, UninstallOutcome::NotInstalled));
        assert_eq!(
            std::fs::read_to_string(&settings_path).unwrap(),
            original,
            "no-op must not rewrite (or even reformat) the file"
        );
        assert!(
            !settings_path
                .with_file_name("settings.json.ng-backup")
                .exists(),
            "no-op must not create a backup"
        );
    }

    #[test]
    fn uninstall_on_missing_file_is_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = uninstall_ng_hooks(&tmp.path().join("settings.json")).unwrap();
        assert!(matches!(outcome, UninstallOutcome::NotInstalled));
    }

    #[test]
    fn uninstall_errors_on_malformed_json_instead_of_clobbering() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        std::fs::write(&settings_path, "not json at all").unwrap();
        assert!(uninstall_ng_hooks(&settings_path).is_err());
        assert_eq!(
            std::fs::read_to_string(&settings_path).unwrap(),
            "not json at all"
        );
    }

    #[test]
    fn is_ng_hook_command_never_matches_lookalikes() {
        assert!(is_ng_hook_command("/usr/local/bin/ng-hook"));
        assert!(is_ng_hook_command("env NG_HARNESS=gemini '/opt/x/ng-hook'"));
        assert!(!is_ng_hook_command("cat /notes/ng-hook.txt"));
        assert!(!is_ng_hook_command("/usr/bin/other-hook"));
        assert!(!is_ng_hook_command("my-tool --flag ng-hookish"));
    }

    #[test]
    fn install_then_uninstall_round_trips_to_the_original_file() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let original = serde_json::to_string_pretty(&serde_json::json!({
            "theme": "dark",
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "/other" }] }] }
        }))
        .unwrap();
        std::fs::write(&settings_path, &original).unwrap();

        install_claude_style(&settings_path, "/bin/ng-hook", DEFAULT_CLAUDE_EVENTS).unwrap();
        uninstall_ng_hooks(&settings_path).unwrap();

        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let before: Value = serde_json::from_str(&original).unwrap();
        assert_eq!(after, before, "uninstall must be install's exact inverse");
    }

    #[test]
    fn gemini_install_uses_gemini_event_names() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("gemini-settings.json");
        install_gemini(&settings_path, "/bin/ng-hook").unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        for event in DEFAULT_GEMINI_EVENTS {
            assert!(value["hooks"][event].is_array());
        }
    }
}

//! End-to-end i18n: roda o binário `ng` de verdade num ambiente hermético
//! (NG_DATA_DIR isolado, sem banco) e confere que a saída user-facing sai no
//! idioma pedido via `NG_LANG`. Cada invocação é um processo próprio, então o
//! `OnceLock` de `Msgs::get` resolve limpo por chamada — sem estado global
//! compartilhado entre os casos.

use std::path::Path;
use std::process::Command;

/// Roda `ng <args>` com um NG_DATA_DIR isolado e o `NG_LANG` dado. Cor
/// desligada (NO_COLOR) para casar substrings sem bytes ANSI no meio.
fn run_ng(lang: &str, data_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ng"))
        .args(args)
        .env("NG_LANG", lang)
        .env("NG_DATA_DIR", data_dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ng")
}

#[test]
fn status_is_english_with_ng_lang_en() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("en", tmp.path(), &["status"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "ng status should exit 0");
    // Rótulo "data" (não "dados") e o banco ainda-inexistente em inglês.
    assert!(stdout.contains("data"), "stdout was: {stdout:?}");
    assert!(!stdout.contains("dados"), "stdout was: {stdout:?}");
    assert!(
        stdout.contains("(not created yet)"),
        "stdout was: {stdout:?}"
    );
    assert!(stdout.contains("database"), "stdout was: {stdout:?}");
}

#[test]
fn status_is_portuguese_with_ng_lang_pt() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("pt", tmp.path(), &["status"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "ng status should exit 0");
    // Rótulo "dados" e o banco ainda-inexistente em português.
    assert!(stdout.contains("dados"), "stdout was: {stdout:?}");
    assert!(
        stdout.contains("(ainda não criado)"),
        "stdout was: {stdout:?}"
    );
    assert!(stdout.contains("banco"), "stdout was: {stdout:?}");
}

#[test]
fn db_missing_error_is_english_with_ng_lang_en() {
    // `ng search` sem banco falha com a mensagem compartilhada db_missing —
    // exercita um caminho de erro (stderr) num comando diferente do status.
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("en", tmp.path(), &["search", "anything"]);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "ng search without a db should fail");
    assert!(
        stderr.contains("database does not exist yet"),
        "stderr was: {stderr:?}"
    );
}

#[test]
fn db_missing_error_is_portuguese_with_ng_lang_pt() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("pt", tmp.path(), &["search", "anything"]);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success(), "ng search without a db should fail");
    assert!(
        stderr.contains("banco não existe ainda"),
        "stderr was: {stderr:?}"
    );
}

#[test]
fn default_without_ng_lang_is_english() {
    // Sem NG_LANG e forçando um locale en, o default internacional aparece.
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ng"))
        .args(["status"])
        .env_remove("NG_LANG")
        .env("LC_ALL", "en_US.UTF-8")
        .env("LANG", "en_US.UTF-8")
        .env("NG_DATA_DIR", tmp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ng");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success());
    assert!(stdout.contains("(not created yet)"), "stdout: {stdout:?}");
}

#[test]
fn locale_pt_selects_portuguese_without_ng_lang() {
    // Detecção por LANG=pt_BR quando NG_LANG está ausente.
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_ng"))
        .args(["status"])
        .env_remove("NG_LANG")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env("LANG", "pt_BR.UTF-8")
        .env("NG_DATA_DIR", tmp.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run ng");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success());
    assert!(stdout.contains("(ainda não criado)"), "stdout: {stdout:?}");
}

// ---- help do clap (about + args), localizado em runtime -----------------
//
// `--help` é um canal separado do parsing: sai sempre com código 0 e o texto
// deve seguir o mesmo idioma da saída interativa. O about da raiz vem no
// `ng --help`; o about de um subcomando e o help de uma flag vêm no
// `ng <sub> --help`. Testamos os dois idiomas para raiz e subcomando+flag.

#[test]
fn help_root_about_is_english_with_ng_lang_en() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("en", tmp.path(), &["--help"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "ng --help should exit 0");
    assert!(
        stdout.contains("universal memory for AI harnesses"),
        "stdout was: {stdout:?}"
    );
    assert!(
        !stdout.contains("memória universal"),
        "en --help must not carry the pt about; stdout was: {stdout:?}"
    );
}

#[test]
fn help_root_about_is_portuguese_with_ng_lang_pt() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("pt", tmp.path(), &["--help"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "ng --help should exit 0");
    assert!(
        stdout.contains("memória universal para harnesses de IA"),
        "stdout was: {stdout:?}"
    );
    assert!(
        !stdout.contains("universal memory for AI harnesses"),
        "pt --help must not carry the en about; stdout was: {stdout:?}"
    );
}

#[test]
fn help_subcommand_and_flag_are_english_with_ng_lang_en() {
    // `ng search --help`: about do subcomando + help de uma flag exclusiva
    // (--semantic) devem sair em inglês.
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("en", tmp.path(), &["search", "--help"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "ng search --help should exit 0");
    assert!(
        stdout.contains("Search the persistent memory"),
        "subcommand about; stdout was: {stdout:?}"
    );
    assert!(
        stdout.contains("rerank by semantic similarity"),
        "flag help; stdout was: {stdout:?}"
    );
    assert!(
        !stdout.contains("Busca na memória persistente"),
        "en search --help must not carry pt text; stdout was: {stdout:?}"
    );
}

#[test]
fn help_subcommand_and_flag_are_portuguese_with_ng_lang_pt() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_ng("pt", tmp.path(), &["search", "--help"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "ng search --help should exit 0");
    assert!(
        stdout.contains("Busca na memória persistente"),
        "subcommand about; stdout was: {stdout:?}"
    );
    assert!(
        stdout.contains("rerank por similaridade semântica"),
        "flag help; stdout was: {stdout:?}"
    );
    assert!(
        !stdout.contains("Search the persistent memory"),
        "pt search --help must not carry en text; stdout was: {stdout:?}"
    );
}

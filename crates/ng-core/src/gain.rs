//! Registro de ganho operacional (`gain_ledger`).
//!
//! Uma linha por injeção servida ou passada de higiene aplicada — métrica
//! operacional, não memória: nada aqui entra em FTS, busca ou injeção.
//! O modelo de honestidade (por que injeção é custo e só higiene é
//! economia) está documentado em `plans/003-ng-context-dir-and-gain.md`.

use serde::{Deserialize, Serialize};

/// Uma linha do `gain_ledger`. A semântica de `tokens` depende de `kind`:
/// - `"inject"`: tokens *injetados* no prompt (custo declarado, ~len/4);
/// - `"evict"` / `"clear"`: tokens *líquidos* removidos do transcript vivo
///   (tokens dos itens stubados menos os tokens dos próprios stubs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GainRecord {
    /// `"inject"` (hook, UserPromptSubmit), `"evict"` (PreCompact) ou
    /// `"clear"` (`ng clear`).
    pub kind: String,
    pub session_id: String,
    /// cwd do evento; `""` quando desconhecido.
    pub project: String,
    pub tokens: i64,
    /// Memórias injetadas ou itens stubados nesta passada.
    pub items: i64,
    pub created_at: i64,
}

/// Envelope de linha de socket para o `ngd`: distingue um registro de ganho
/// de um [`crate::Event`] pelo campo `ng_gain` (um `Event` nunca o tem, um
/// envelope nunca tem os campos obrigatórios de `Event` — os dois parses
/// jamais se confundem).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GainEnvelope {
    pub ng_gain: GainRecord,
}

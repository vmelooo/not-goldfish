//! ng-core: storage, lexical tagging and search for not-goldfish.
//!
//! Single source of truth is a SQLite database (WAL mode) with an FTS5
//! index. Everything captured from a harness session is stored as an
//! [`Event`]; nothing is ever deleted, only superseded.

pub mod embed;
#[cfg(feature = "model2vec")]
pub mod embed_model2vec;
pub mod event;
pub mod gain;
pub mod graph;
pub mod lex;
pub mod paths;
pub mod saver;
pub mod store;
pub mod timeutil;

pub use embed::{cosine, default_embedder, Embedder, HashEmbedder};
pub use event::Event;
pub use gain::{GainEnvelope, GainRecord};
pub use graph::extract_entities;
pub use saver::{Compressed, Saver, SaverRef};
pub use store::{Entity, Memory, PendingImport, PendingScan, SearchHit, Store};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

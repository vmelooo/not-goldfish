//! ng-bench: a reproducible, offline "with vs without not-goldfish" study.
//!
//! Consumes ng-core's public API only. No network, no live LLM, no clock, no
//! randomness — the corpus and every metric are deterministic, so this crate
//! doubles as a CI regression gate on retrieval quality and token savings.
//!
//! See `docs/benchmarks/with-vs-without.md` for methodology and results.

pub mod corpus;
pub mod harness;

pub use corpus::{build_corpus, Corpus, Task, TaskClass};
pub use harness::{run_full_study, ArmSummary, ClassResults, StudyResults, TaskMetric, TOP_K};

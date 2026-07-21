//! Wisdom-graph ingestion, traversal, snapshots and Markdown export.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection};

use super::util::now_epoch;
use super::{
    Entity, Store, DECISION_INITIAL_WEIGHT, DEFAULT_INITIAL_WEIGHT, GRAPH_REINFORCE_STEP,
    MAX_GRAPH_WEIGHT,
};
use crate::event::Event;
use crate::graph;
use crate::Result;

/// Bump this whenever entity-extraction rules change shape: the stored
/// version mismatching it wipes the derived graph on next open so the
/// worker re-ingests history under the new rules. v2 = dialogue-only
/// lexical layer + typed tool entities (fase grafo-saneado).
pub const GRAPH_RULES_VERSION: i64 = 2;

/// Compare stored rules version with [`GRAPH_RULES_VERSION`]; on mismatch
/// (including "never stored", which covers every pre-existing database)
/// wipe entities/relations and reset the cursor, all in one transaction.
pub(super) fn ensure_rules_version(conn: &Connection) -> Result<()> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM graph_meta WHERE key = 'rules_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    if stored.as_deref() == Some(&GRAPH_RULES_VERSION.to_string()) {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM relations", [])?;
    tx.execute("DELETE FROM entities", [])?;
    tx.execute("UPDATE graph_cursor SET last_event = 0 WHERE id = 1", [])?;
    tx.execute(
        "INSERT INTO graph_meta (key, value) VALUES ('rules_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![GRAPH_RULES_VERSION.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

impl Store {
    /// Wipe the derived graph and re-ingest the entire event history
    /// synchronously. Batches keep transactions bounded; returns total
    /// events processed. `events` is never touched.
    ///
    /// A rebuild that drains to completion stamps [`GRAPH_RULES_VERSION`]
    /// into `graph_meta` — deliberately only at the end, never in the wipe
    /// transaction: an interrupted rebuild keeps the old/absent stamp, so
    /// the next open re-wipes and re-ingests instead of trusting a
    /// half-built graph as current. The `CREATE TABLE IF NOT EXISTS`
    /// covers the CLI path (`open_rw_no_init` skips `init`): on a database
    /// last initialized by an older binary, `graph_meta` may not exist yet.
    pub fn graph_rebuild(&self) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM relations", [])?;
        tx.execute("DELETE FROM entities", [])?;
        tx.execute("UPDATE graph_cursor SET last_event = 0 WHERE id = 1", [])?;
        tx.commit()?;
        const REBUILD_BATCH: usize = 500;
        let mut total = 0;
        loop {
            let n = self.graph_ingest_pending(REBUILD_BATCH)?;
            total += n;
            if n < REBUILD_BATCH {
                break;
            }
        }
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS graph_meta (
                key    TEXT PRIMARY KEY,
                value  TEXT NOT NULL
            )",
            [],
        )?;
        self.conn.execute(
            "INSERT INTO graph_meta (key, value) VALUES ('rules_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![GRAPH_RULES_VERSION.to_string()],
        )?;
        Ok(total)
    }

    /// Fold one event into the wisdom graph: upsert its extracted entities
    /// (reinforcing weight on reappearance) and connect every pair of
    /// entities from the same event with a `cooccurs` relation. All
    /// entities are scoped to `event.project` — the graph is per-project by
    /// default; `neighbors`/`export_graph_md` additionally pull in the
    /// global (`project = ''`) scope for cross-project entities added by
    /// other means.
    pub fn ingest_graph(&self, event: &Event) -> Result<()> {
        ingest_graph_conn(&self.conn, event)
    }

    /// Nudge every entity named `name` by `delta` (clamped to
    /// `[0, MAX_GRAPH_WEIGHT]`) — a hook for future feedback signals (e.g.
    /// "this suggestion was useful") independent of graph ingestion.
    /// Returns how many entities were touched.
    pub fn bump_entity(&self, name: &str, delta: f64) -> Result<usize> {
        let now = now_epoch();
        let touched = self.conn.execute(
            "UPDATE entities SET weight = MAX(0.0, MIN(?1, weight + ?2)), updated_at = ?3 WHERE name = ?4",
            params![MAX_GRAPH_WEIGHT, delta, now, name],
        )?;
        Ok(touched)
    }

    /// Weighted BFS out to `depth` hops (clamped to 2) from the entity
    /// named `name`, scoped to `project` plus the global (`''`) scope.
    /// Score of a reached entity is `entity.weight * accumulated path
    /// weight`, where each hop contributes `edge.weight / MAX_GRAPH_WEIGHT`
    /// (a fresh, unreinforced edge contributes 0.1) — normalizing against
    /// the shared cap turns edge weight into a per-hop damping factor, so
    /// a 2nd-degree match naturally scores below a 1st-degree one even
    /// though raw weights only ever grow, never shrink. Returns entities
    /// sorted by score descending, truncated to `limit`. Empty if `name`
    /// isn't found.
    pub fn neighbors(
        &self,
        name: &str,
        project: Option<&str>,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<(Entity, f64)>> {
        let depth = depth.min(2);
        let entities = self.load_scoped_entities(project)?;
        let Some(start) = entities.iter().find(|e| e.name == name) else {
            return Ok(Vec::new());
        };
        let ids: HashSet<i64> = entities.iter().map(|e| e.id).collect();
        let adjacency = self.load_adjacency(&ids)?;
        let by_id: HashMap<i64, &Entity> = entities.iter().map(|e| (e.id, e)).collect();

        // best_path[node] = highest accumulated path weight of any path
        // from `start` to `node` found within `depth` hops.
        let mut best_path: HashMap<i64, f64> = HashMap::new();
        best_path.insert(start.id, 1.0);
        let mut frontier: Vec<(i64, f64)> = vec![(start.id, 1.0)];
        for _ in 0..depth {
            let mut next = Vec::new();
            for (node, acc) in &frontier {
                let Some(edges) = adjacency.get(node) else {
                    continue;
                };
                for (neighbor, edge_weight) in edges {
                    let candidate = acc * (edge_weight / MAX_GRAPH_WEIGHT);
                    let best = best_path.entry(*neighbor).or_insert(0.0);
                    if candidate > *best {
                        *best = candidate;
                        next.push((*neighbor, candidate));
                    }
                }
            }
            frontier = next;
        }
        best_path.remove(&start.id);

        let mut results: Vec<(Entity, f64)> = best_path
            .into_iter()
            .filter_map(|(id, acc)| by_id.get(&id).map(|e| ((*e).clone(), e.weight * acc)))
            .collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    /// Entities and (deduped, undirected) edges for a graph view, meant for
    /// callers that just need to render/serialize — the UI's `/api/graph`.
    /// Without `focus`: the top `limit` entities by weight in scope
    /// (`project` + global), plus edges between them. With `focus`: the
    /// focal entity plus up to `limit - 1` of its [`Store::neighbors`] out
    /// to `depth` hops, plus edges among that set. Empty (not an error)
    /// when `focus` doesn't resolve to a known entity.
    pub fn graph_snapshot(
        &self,
        project: Option<&str>,
        focus: Option<&str>,
        depth: usize,
        limit: usize,
    ) -> Result<(Vec<Entity>, Vec<WeightedEdge>)> {
        let nodes = if let Some(name) = focus {
            let entities = self.load_scoped_entities(project)?;
            let Some(start) = entities.iter().find(|e| e.name == name).cloned() else {
                return Ok((Vec::new(), Vec::new()));
            };
            let mut nodes = vec![start];
            let neighbors = self.neighbors(name, project, depth, limit.saturating_sub(1))?;
            nodes.extend(neighbors.into_iter().map(|(entity, _)| entity));
            nodes
        } else {
            let mut entities = self.load_scoped_entities(project)?;
            entities.sort_by(|a, b| {
                b.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            entities.truncate(limit);
            entities
        };

        let ids: HashSet<i64> = nodes.iter().map(|e| e.id).collect();
        let adjacency = self.load_adjacency(&ids)?;
        Ok((nodes, dedupe_edges(&adjacency)))
    }

    /// Entities scoped to `project` (plus the global `''` scope), or every
    /// entity when `project` is `None`.
    pub(super) fn load_scoped_entities(&self, project: Option<&str>) -> Result<Vec<Entity>> {
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Entity> {
            Ok(Entity {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
                project: row.get(3)?,
                weight: row.get(4)?,
                updated_at: row.get(5)?,
            })
        };
        let mut out = Vec::new();
        if let Some(p) = project {
            let mut stmt = self.conn.prepare(
                "SELECT id, name, kind, project, weight, updated_at FROM entities WHERE project = ?1 OR project = ''",
            )?;
            let rows = stmt.query_map(params![p], map)?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let mut stmt = self
                .conn
                .prepare("SELECT id, name, kind, project, weight, updated_at FROM entities")?;
            let rows = stmt.query_map([], map)?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// Undirected `cooccurs` adjacency restricted to `ids` on both ends.
    /// Filters `a IN (...) AND b IN (...)` in SQL (backed by
    /// `idx_relations_kind`) instead of pulling the entire `relations`
    /// table into Rust and filtering there — callers like `neighbors` and
    /// `graph_snapshot` only ever need edges among a small scoped id set.
    /// The id set is queried in chunks: a single `IN (...)` with tens of
    /// thousands of ids (a banco real acumulado) estoura o limite de
    /// variáveis do SQLite ("too many SQL variables").
    pub(super) fn load_adjacency(
        &self,
        ids: &HashSet<i64>,
    ) -> Result<HashMap<i64, Vec<(i64, f64)>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        /// ids por query (2× isso em variáveis bindadas — abaixo de
        /// qualquer SQLITE_MAX_VARIABLE_NUMBER).
        const ADJACENCY_CHUNK: usize = 400;

        let id_list: Vec<i64> = ids.iter().copied().collect();
        let mut adjacency: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
        for chunk in id_list.chunks(ADJACENCY_CHUNK) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT a, b, weight FROM relations
                 WHERE kind = 'cooccurs' AND a IN ({placeholders}) AND b IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut sql_params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(chunk.len() * 2);
            for id in chunk {
                sql_params.push(Box::new(*id));
            }
            for id in chunk {
                sql_params.push(Box::new(*id));
            }
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                sql_params.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })?;
            for row in rows {
                let (a, b, w) = row?;
                adjacency.entry(a).or_default().push((b, w));
                adjacency.entry(b).or_default().push((a, w));
            }
        }
        Ok(adjacency)
    }

    /// Render the graph as Markdown: one section per entity kind, entities
    /// sorted by weight descending, each with its top-3 strongest
    /// relations — meant to be pasted or auto-injected into
    /// CLAUDE.md/AGENTS.md as durable project memory.
    pub fn export_graph_md(&self, project: Option<&str>) -> Result<String> {
        let entities = self.load_scoped_entities(project)?;
        let ids: HashSet<i64> = entities.iter().map(|e| e.id).collect();
        let adjacency = self.load_adjacency(&ids)?;
        let by_id: HashMap<i64, &Entity> = entities.iter().map(|e| (e.id, e)).collect();

        let mut out = String::new();
        out.push_str("# Grafo de sabedoria\n\n");
        for kind in ["decision", "error", "file", "concept"] {
            let mut group: Vec<&Entity> = entities.iter().filter(|e| e.kind == kind).collect();
            if group.is_empty() {
                continue;
            }
            group.sort_by(|a, b| {
                b.weight
                    .partial_cmp(&a.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            out.push_str(&format!("## {}\n\n", section_title(kind)));
            for entity in group {
                out.push_str(&format!(
                    "- **{}** (peso {:.1})\n",
                    entity.name, entity.weight
                ));
                if let Some(edges) = adjacency.get(&entity.id) {
                    let mut edges = edges.clone();
                    edges
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                    for (neighbor_id, weight) in edges.into_iter().take(3) {
                        if let Some(neighbor) = by_id.get(&neighbor_id) {
                            out.push_str(&format!(
                                "  - relaciona com **{}** (peso {:.1})\n",
                                neighbor.name, weight
                            ));
                        }
                    }
                }
            }
            out.push('\n');
        }
        Ok(out)
    }

    /// Ingest up to `limit` events newer than the graph cursor, advancing
    /// it past the highest id processed. Idempotent: calling again with no
    /// new events since the last run does nothing and returns 0.
    ///
    /// The whole batch (every entity/relation upsert plus the cursor
    /// advance) runs inside one transaction. Without this, a failure on
    /// event k of the batch would leave events 0..k-1's weight increments
    /// committed but the cursor unmoved — the next poll reprocesses
    /// 0..k-1, reinforcing their weights a second time for events that
    /// were never actually lost. Wrapping in a transaction makes a partial
    /// failure an all-or-nothing rollback: either the whole batch's
    /// weights AND the cursor advance together, or neither does.
    pub fn graph_ingest_pending(&self, limit: usize) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let last_event: i64 = tx.query_row(
            "SELECT last_event FROM graph_cursor WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        let mut stmt = tx.prepare(
            "SELECT id, session_id, project, harness, kind, content, tags, created_at, meta
             FROM events WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![last_event, limit as i64], |row| {
            let id: i64 = row.get(0)?;
            let event = Event {
                session_id: row.get(1)?,
                project: row.get(2)?,
                harness: row.get(3)?,
                kind: row.get(4)?,
                content: row.get(5)?,
                tags: row.get(6)?,
                created_at: row.get(7)?,
                meta: row.get(8)?,
            };
            Ok((id, event))
        })?;
        let mut batch = Vec::new();
        for row in rows {
            batch.push(row?);
        }
        drop(stmt);

        let mut max_id = last_event;
        for (id, event) in &batch {
            ingest_graph_conn(&tx, event)?;
            max_id = max_id.max(*id);
        }
        if max_id != last_event {
            tx.execute(
                "UPDATE graph_cursor SET last_event = ?1 WHERE id = 1",
                params![max_id],
            )?;
        }
        tx.commit()?;
        Ok(batch.len())
    }
}

/// Shared implementation of [`Store::ingest_graph`], parameterized over the
/// connection so [`Store::graph_ingest_pending`] can run the whole batch
/// (all of this plus the cursor advance) inside a single transaction.
fn ingest_graph_conn(conn: &Connection, event: &Event) -> Result<()> {
    let extracted = graph::extract_entities(event);
    if extracted.is_empty() {
        return Ok(());
    }
    let mut ids = Vec::with_capacity(extracted.len());
    for (name, kind) in &extracted {
        let initial_weight = if kind == "decision" {
            DECISION_INITIAL_WEIGHT
        } else {
            DEFAULT_INITIAL_WEIGHT
        };
        ids.push(upsert_entity_conn(
            conn,
            name,
            kind,
            &event.project,
            initial_weight,
            event.created_at,
        )?);
    }
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            upsert_relation_conn(conn, ids[i], ids[j], "cooccurs", event.created_at)?;
        }
    }
    Ok(())
}

fn upsert_entity_conn(
    conn: &Connection,
    name: &str,
    kind: &str,
    project: &str,
    initial_weight: f64,
    now: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO entities (name, kind, project, weight, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name, kind, project) DO UPDATE SET
            weight = MIN(?6, weight + ?7), updated_at = ?5",
        params![
            name,
            kind,
            project,
            initial_weight,
            now,
            MAX_GRAPH_WEIGHT,
            GRAPH_REINFORCE_STEP
        ],
    )?;
    conn.query_row(
        "SELECT id FROM entities WHERE name = ?1 AND kind = ?2 AND project = ?3",
        params![name, kind, project],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

fn upsert_relation_conn(conn: &Connection, a: i64, b: i64, kind: &str, now: i64) -> Result<()> {
    if a == b {
        return Ok(()); // no self-loops (duplicate entities in one event)
    }
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    conn.execute(
        "INSERT INTO relations (a, b, kind, weight, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(a, b, kind) DO UPDATE SET
            weight = MIN(?6, weight + ?7), updated_at = ?5",
        params![
            a,
            b,
            kind,
            DEFAULT_INITIAL_WEIGHT,
            now,
            MAX_GRAPH_WEIGHT,
            GRAPH_REINFORCE_STEP
        ],
    )?;
    Ok(())
}

fn section_title(kind: &str) -> &'static str {
    match kind {
        "decision" => "Decisões",
        "error" => "Erros",
        "file" => "Arquivos",
        "concept" => "Conceitos",
        _ => "Outros",
    }
}

/// `(entity_a_id, entity_b_id, weight)` — an undirected graph edge, `a <= b`.
type WeightedEdge = (i64, i64, f64);

/// Flattens the doubled undirected adjacency map (`load_adjacency` stores
/// each edge on both endpoints) into one `(a, b, weight)` triple per edge,
/// `a <= b`, so callers serializing to JSON don't emit each edge twice.
fn dedupe_edges(adjacency: &HashMap<i64, Vec<(i64, f64)>>) -> Vec<WeightedEdge> {
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut edges = Vec::new();
    for (&a, neighbors) in adjacency {
        for &(b, weight) in neighbors {
            let key = if a <= b { (a, b) } else { (b, a) };
            if seen.insert(key) {
                edges.push((key.0, key.1, weight));
            }
        }
    }
    edges
}

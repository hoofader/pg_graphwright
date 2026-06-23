// pg_graphwright — a knowledge-graph index for Postgres.
//
// The graph is derived from documents (rows of a watched table) and kept
// in catalog tables under the `graphwright` schema. The point that no
// other system has: a graph element's visibility follows the row-level
// security of the source rows it was derived from. The accessors below
// probe the source table as the calling user, so Postgres' own RLS
// decides which rows the user can read, and the graph is filtered to
// match. No bespoke access-control engine; we delegate to RLS.
//
// Extraction is a deterministic stub for now (tokenize a row, co-mention
// edges); a real LLM/GLiNER extension point comes later. That changes how the graph
// is filled, not how it is filtered. `CREATE INDEX ... USING graphwright`
// drives the build through the index access method below.

use pgrx::prelude::*;

pgrx::pg_module_magic!(name, version);

mod resolve;
use resolve::{
    canonical_map, gated_fuzzy_pairs, gated_phonetic_pairs, normalize_name, phonetic_keys,
};
use std::collections::HashSet;

// Two normalized names, ordered (norm_a <= norm_b): a merge/split decision.
type NormPair = (String, String);

fn lit(s: &str) -> String {
    pgrx::spi::quote_literal(s)
}
fn ident(s: &str) -> String {
    pgrx::spi::quote_identifier(s)
}

// Split a row into normalized tokens, deduplicated within the row,
// first-occurrence order kept (so co-mention edges are stable). Unicode
// alphanumerics survive, so Persian text tokenizes the same as Latin.
fn tokenize(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let norm = raw.to_lowercase();
        if seen.insert(norm.clone()) {
            out.push(norm);
        }
    }
    out
}

// Extract entity surfaces from a document, then pass them through the
// judge. Both are pluggable SQL functions, so the extension stays model-
// agnostic, the way graphwright's core treats the LLM as an injected extension point.
// Runs where extraction is scheduled (async, off the writing transaction).
fn extract(text: &str) -> Vec<String> {
    judge(text, run_extractor(text))
}

// The extractor extension point: graphwright.extractor names a function
// `f(text) -> text[]`. Empty means the built-in tokenizer (no model). The
// host wires in GLiNER via graphwright-onnx, an LLM gateway, a regex NER.
fn run_extractor(text: &str) -> Vec<String> {
    let Some(name) = EXTRACTOR.get() else {
        return tokenize(text);
    };
    let name = name.to_string_lossy();
    if name.trim().is_empty() {
        return tokenize(text);
    }
    // name is an admin-set GUC (a function name), interpolated as-is.
    // A failing extractor (bad SQL, type mismatch, HTTP timeout for the
    // gliner path) must not silently empty the graph for that row: warn so
    // the operator sees a misconfigured extractor instead of missing data.
    let arr =
        match pgrx::Spi::get_one::<Vec<Option<String>>>(&format!("SELECT {}({})", name, lit(text)))
        {
            Ok(v) => v.unwrap_or_default(),
            Err(e) => {
                pgrx::warning!(
                    "graphwright: extractor {name} failed, treating as no surfaces: {e}"
                );
                Vec::new()
            }
        };
    arr.into_iter().flatten().collect()
}

// The judge extension point: graphwright.judge names a function
// `j(text, text[]) -> text[]`, a larger model that validates or trims the
// extractor's output before it reaches the graph. AI output is never
// canon; this is where the bigger model disposes. A judge error or NULL
// keeps the extractor's output unchanged.
fn judge(text: &str, surfaces: Vec<String>) -> Vec<String> {
    let Some(name) = JUDGE.get() else {
        return surfaces;
    };
    let name = name.to_string_lossy();
    if name.trim().is_empty() {
        return surfaces;
    }
    let array = if surfaces.is_empty() {
        "ARRAY[]::text[]".to_string()
    } else {
        format!(
            "ARRAY[{}]::text[]",
            surfaces
                .iter()
                .map(|s| lit(s))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    // A judge error or NULL keeps the extractor's output unchanged, but a
    // raised error is a misconfiguration the operator should see.
    match pgrx::Spi::get_one::<Vec<Option<String>>>(&format!(
        "SELECT {}({}, {})",
        name,
        lit(text),
        array
    )) {
        Ok(Some(judged)) => judged.into_iter().flatten().collect(),
        Ok(None) => surfaces,
        Err(e) => {
            pgrx::warning!("graphwright: judge {name} failed, keeping extractor output: {e}");
            surfaces
        }
    }
}

// The relation extension point: graphwright.relation_extractor names a
// function `f(text) -> text[]`, a flat list of (subject, predicate, object)
// triples. When set, edges are the directed, typed relations it returns;
// empty falls back to undirected co-mention. Endpoints resolve to entities
// the same way surfaces do, so only relations between extracted entities
// become edges. The model proposes; a human can still split or merge.
fn run_relation_extractor(text: &str) -> Vec<(String, String, String)> {
    let Some(name) = RELATION_EXTRACTOR.get() else {
        return Vec::new();
    };
    let name = name.to_string_lossy();
    if name.trim().is_empty() {
        return Vec::new();
    }
    let flat =
        match pgrx::Spi::get_one::<Vec<Option<String>>>(&format!("SELECT {}({})", name, lit(text)))
        {
            Ok(v) => v.unwrap_or_default(),
            Err(e) => {
                pgrx::warning!(
                    "graphwright: relation extractor {name} failed, treating as no relations: {e}"
                );
                Vec::new()
            }
        };
    let flat: Vec<String> = flat.into_iter().flatten().collect();
    flat.chunks_exact(3)
        .map(|c| (c[0].clone(), c[1].clone(), c[2].clone()))
        .collect()
}

fn relation_extractor_set() -> bool {
    RELATION_EXTRACTOR
        .get()
        .map(|n| !n.to_string_lossy().trim().is_empty())
        .unwrap_or(false)
}

// The embedding extension point: graphwright.embedder names a function
// `f(text) -> float8[]`. It embeds each norm and merges pairs whose cosine
// clears graphwright.embedding_threshold, rescuing short names the lexical
// gate dropped. Empty extension point leaves the deterministic behavior unchanged.
fn embed_pairs(norms: &HashSet<String>) -> Vec<(String, String)> {
    let Some(name) = EMBEDDER.get() else {
        return Vec::new();
    };
    let name = name.to_string_lossy().trim().to_string();
    if name.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&String> = norms.iter().collect();
    sorted.sort();
    let mut vectors: Vec<(String, Vec<f64>)> = Vec::new();
    for n in sorted {
        let vec = pgrx::Spi::get_one::<Vec<Option<f64>>>(&format!("SELECT {}({})", name, lit(n)))
            .ok()
            .flatten()
            .map(|xs| xs.into_iter().flatten().collect::<Vec<f64>>())
            .unwrap_or_default();
        if !vec.is_empty() {
            vectors.push((n.clone(), vec));
        }
    }
    resolve::embedding_pairs(&vectors, EMBEDDING_THRESHOLD.get())
}

enum Visibility {
    Union,
    Intersection,
}

struct WatchMeta {
    id: i32,
    source_table: String,
    pk_column: String,
    visibility: Visibility,
}

// Look up a registered watch by the source table name.
fn watch_meta(source_table: &str) -> WatchMeta {
    pgrx::Spi::connect(|client| {
        let sql = format!(
            "SELECT id, source_table::text, pk_column, visibility \
             FROM graphwright.watch WHERE source_table = {}::regclass",
            lit(source_table),
        );
        let table = client.select(&sql, Some(1), &[])?;
        let row = table.first();
        let id = row.get::<i32>(1)?.expect("watch id");
        let source_table = row.get::<String>(2)?.expect("source table");
        let pk_column = row.get::<String>(3)?.expect("pk column");
        let visibility = match row.get::<String>(4)?.as_deref() {
            Some("intersection") => Visibility::Intersection,
            _ => Visibility::Union,
        };
        Ok::<_, pgrx::spi::Error>(WatchMeta {
            id,
            source_table,
            pk_column,
            visibility,
        })
    })
    .expect("watch_meta")
}

// Register (or update) a watch and return its id. The index AM passes
// pk_column = "ctid", which the RLS accessors then probe as `(s.ctid)::text`.
fn upsert_watch(source_table: &str, text_column: &str, pk_column: &str) -> i32 {
    pgrx::Spi::get_one::<i32>(&format!(
        "INSERT INTO graphwright.watch (source_table, text_column, pk_column) \
         VALUES ({}::regclass, {}, {}) \
         ON CONFLICT (source_table, text_column) \
         DO UPDATE SET pk_column = EXCLUDED.pk_column \
         RETURNING id",
        lit(source_table),
        lit(text_column),
        lit(pk_column),
    ))
    .expect("watch insert")
    .expect("watch id")
}

// Install the change-capture trigger on a watch's table (the no-index
// path; the index path captures changes in aminsert instead).
fn install_capture_trigger(watch_id: i32) {
    use pgrx::Spi;
    let table = Spi::get_one::<String>(&format!(
        "SELECT source_table::text FROM graphwright.watch WHERE id = {watch_id}"
    ))
    .expect("watch table")
    .expect("watch table");
    Spi::run(&format!(
        "CREATE OR REPLACE TRIGGER graphwright_capture \
         AFTER INSERT OR UPDATE OR DELETE ON {table} \
         FOR EACH ROW EXECUTE FUNCTION graphwright._enqueue()"
    ))
    .expect("install capture trigger");
}

// Clear a watch's resolved graph, then resolve it from the per-row tokens
// stored in the index relation. This makes index storage the source of
// truth for the index path.
fn resolve_from_storage(indexrel: pg_sys::Relation, watch_id: i32) {
    use pgrx::Spi;

    // Read the mentions, then decide entity identity before writing the
    // graph: collect norms, merge what decisions and gated phonetic
    // matches link, and resolve to canonical norms.
    let records: Vec<(String, Vec<String>)> = unsafe { storage::scan(indexrel) }
        .iter()
        .filter_map(|rec| {
            let (tag, block, offset, surfaces) = storage::decode(rec);
            (tag == MENTIONS).then(|| (format!("({block},{offset})"), surfaces))
        })
        .collect();

    let mut norms: HashSet<String> = HashSet::new();
    for (_, surfaces) in &records {
        for s in surfaces {
            let n = normalize_name(s);
            if !n.is_empty() {
                norms.insert(n);
            }
        }
    }
    let (manual, splits) = read_decisions(watch_id);
    let split_set: HashSet<(String, String)> = splits.into_iter().collect();
    let merges: Vec<(String, String)> = manual
        .into_iter()
        .chain(gated_phonetic_pairs(&norms))
        .chain(gated_fuzzy_pairs(&norms))
        .chain(embed_pairs(&norms))
        .filter(|p| !split_set.contains(p))
        .collect();
    let canon = canonical_map(&norms, &merges);
    let overrides = read_overrides(watch_id);

    // Typed edges need the row text, which is not in index storage. When a
    // relation extractor is set, re-read each row's body by ctid to run it;
    // otherwise edges are co-mention over the stored tokens.
    let text_source: Option<(String, String)> = relation_extractor_set()
        .then(|| {
            Spi::get_two::<String, String>(&format!(
                "SELECT source_table::text, text_column FROM graphwright.watch WHERE id = {watch_id}"
            ))
            .ok()
            .and_then(|(t, c)| Some((t?, c?)))
        })
        .flatten();

    Spi::run(&format!(
        "DELETE FROM graphwright.entity WHERE watch_id = {watch_id}"
    ))
    .expect("clear watch");
    for (ctid, surfaces) in &records {
        let relations = text_source.as_ref().map(|(tbl, col)| {
            let body = Spi::get_one::<String>(&format!(
                "SELECT ({col})::text FROM {tbl} WHERE ctid = '{ctid}'::tid",
                col = ident(col),
            ))
            .ok()
            .flatten()
            .unwrap_or_default();
            run_relation_extractor(&body)
        });
        resolve_tokens(
            watch_id,
            ctid,
            surfaces,
            &canon,
            &overrides,
            relations.as_deref(),
        );
    }
}

// Read a watch's durable decisions as sorted norm pairs: (merges, splits).
fn read_decisions(watch_id: i32) -> (Vec<NormPair>, Vec<NormPair>) {
    use pgrx::Spi;
    let rows: Vec<(String, String, String)> = Spi::connect(|client| {
        let table = client.select(
            &format!(
                "SELECT norm_a, norm_b, verdict FROM graphwright.decision WHERE watch_id = {watch_id}"
            ),
            None,
            &[],
        )?;
        let mut out = Vec::new();
        for row in table {
            out.push((
                row.get::<String>(1)?.expect("norm_a"),
                row.get::<String>(2)?.expect("norm_b"),
                row.get::<String>(3)?.expect("verdict"),
            ));
        }
        Ok::<_, pgrx::spi::Error>(out)
    })
    .unwrap_or_default();
    let mut merges = Vec::new();
    let mut splits = Vec::new();
    for (a, b, verdict) in rows {
        if verdict == "merge" {
            merges.push((a, b));
        } else {
            splits.push((a, b));
        }
    }
    (merges, splits)
}

fn watch_id_of(source_table: &str) -> i32 {
    pgrx::Spi::get_one::<i32>(&format!(
        "SELECT id FROM graphwright.watch WHERE source_table = {}::regclass",
        lit(source_table)
    ))
    .expect("watch lookup")
    .expect("no watch for table")
}

// Record a durable merge/split decision (normalized, ordered), then
// re-resolve so it takes effect now. Reversible: drop_decision removes it.
fn record_decision(source_table: &str, a: &str, b: &str, verdict: &str) -> bool {
    let wid = watch_id_of(source_table);
    let (na, nb) = (normalize_name(a), normalize_name(b));
    if na.is_empty() || nb.is_empty() || na == nb {
        return false;
    }
    let (lo, hi) = if na < nb { (na, nb) } else { (nb, na) };
    pgrx::Spi::run(&format!(
        "INSERT INTO graphwright.decision (watch_id, norm_a, norm_b, verdict) \
         VALUES ({wid}, {a}, {b}, {v}) \
         ON CONFLICT (watch_id, norm_a, norm_b) \
         DO UPDATE SET verdict = EXCLUDED.verdict, decided_by = current_user, decided_at = now()",
        a = lit(&lo),
        b = lit(&hi),
        v = lit(verdict),
    ))
    .expect("record decision");
    drain_all();
    true
}

fn drop_decision(source_table: &str, a: &str, b: &str) -> bool {
    let wid = watch_id_of(source_table);
    let (na, nb) = (normalize_name(a), normalize_name(b));
    let (lo, hi) = if na < nb { (na, nb) } else { (nb, na) };
    pgrx::Spi::run(&format!(
        "DELETE FROM graphwright.decision WHERE watch_id = {wid} AND norm_a = {a} AND norm_b = {b}",
        a = lit(&lo),
        b = lit(&hi),
    ))
    .expect("drop decision");
    drain_all();
    true
}

fn list_decisions(source_table: &str) -> Vec<(String, String, String)> {
    let wid = watch_id_of(source_table);
    pgrx::Spi::connect(|client| {
        let table = client.select(
            &format!(
                "SELECT norm_a, norm_b, verdict FROM graphwright.decision \
                 WHERE watch_id = {wid} ORDER BY norm_a, norm_b"
            ),
            None,
            &[],
        )?;
        let mut out = Vec::new();
        for row in table {
            out.push((
                row.get::<String>(1)?.unwrap(),
                row.get::<String>(2)?.unwrap(),
                row.get::<String>(3)?.unwrap(),
            ));
        }
        Ok::<_, pgrx::spi::Error>(out)
    })
    .unwrap_or_default()
}

// Per-mention overrides for a watch, keyed by (source_pk, surface_norm).
fn read_overrides(watch_id: i32) -> std::collections::HashMap<(String, String), String> {
    pgrx::Spi::connect(|client| {
        let table = client.select(
            &format!(
                "SELECT source_pk, surface_norm, tag FROM graphwright.mention_override \
                 WHERE watch_id = {watch_id}"
            ),
            None,
            &[],
        )?;
        let mut out = std::collections::HashMap::new();
        for row in table {
            let pk = row.get::<String>(1)?.expect("source_pk");
            let norm = row.get::<String>(2)?.expect("surface_norm");
            let tag = row.get::<String>(3)?.expect("tag");
            out.insert((pk, norm), tag);
        }
        Ok::<_, pgrx::spi::Error>(out)
    })
    .unwrap_or_default()
}

// Pin one surface occurrence in one row to a private entity, then re-resolve.
// The tag groups occurrences into the same private entity; empty defaults to
// the row, so each split stands alone. Reversible: drop_mention_override.
fn record_mention_override(source_table: &str, source_pk: &str, surface: &str, tag: &str) -> bool {
    let wid = watch_id_of(source_table);
    let norm = normalize_name(surface);
    if norm.is_empty() {
        return false;
    }
    let tag = if tag.is_empty() { source_pk } else { tag };
    pgrx::Spi::run(&format!(
        "INSERT INTO graphwright.mention_override (watch_id, source_pk, surface_norm, tag) \
         VALUES ({wid}, {pk}, {norm}, {tag}) \
         ON CONFLICT (watch_id, source_pk, surface_norm) \
         DO UPDATE SET tag = EXCLUDED.tag, decided_by = current_user, decided_at = now()",
        pk = lit(source_pk),
        norm = lit(&norm),
        tag = lit(tag),
    ))
    .expect("record mention override");
    drain_all();
    true
}

fn drop_mention_override(source_table: &str, source_pk: &str, surface: &str) -> bool {
    let wid = watch_id_of(source_table);
    let norm = normalize_name(surface);
    pgrx::Spi::run(&format!(
        "DELETE FROM graphwright.mention_override \
         WHERE watch_id = {wid} AND source_pk = {pk} AND surface_norm = {norm}",
        pk = lit(source_pk),
        norm = lit(&norm),
    ))
    .expect("drop mention override");
    drain_all();
    true
}

// Extract every pending row (markers in index storage), writing the
// resulting mentions back to storage. This is where the (possibly slow)
// extractor + judge run, off the writing transaction. Runs privileged so
// it sees every row.
fn extract_pending(indexrel: pg_sys::Relation, watch_id: i32) {
    use pgrx::Spi;
    let (source_table, text_column) = Spi::get_two::<String, String>(&format!(
        "SELECT source_table::text, text_column FROM graphwright.watch WHERE id = {watch_id}"
    ))
    .expect("watch lookup");
    let source_table = source_table.expect("source table");
    let text_column = text_column.expect("text column");

    let markers: Vec<(u32, u16)> = unsafe { storage::scan(indexrel) }
        .iter()
        .filter_map(|r| {
            let (tag, b, o, _) = storage::decode(r);
            (tag == MARKER).then_some((b, o))
        })
        .collect();
    if markers.is_empty() {
        return;
    }

    let mut extracted: Vec<(u32, u16, Vec<String>)> = Vec::new();
    for (block, offset) in &markers {
        let text = Spi::get_one::<String>(&format!(
            "SELECT ({txt})::text FROM {tbl} WHERE ctid = '({block},{offset})'::tid",
            txt = ident(&text_column),
            tbl = source_table,
        ))
        .ok()
        .flatten();
        if let Some(text) = text {
            extracted.push((*block, *offset, extract(&text)));
        }
    }

    // Mark the markers done (only markers exist for these ctids yet), then
    // append the mentions. Ordering matters: prune before append.
    let done: std::collections::HashSet<(u32, u16)> = markers.into_iter().collect();
    unsafe {
        storage::prune(indexrel, &mut |b, o| done.contains(&(b, o)));
        for (block, offset, surfaces) in &extracted {
            storage::append(indexrel, &storage::mentions(*block, *offset, surfaces));
        }
    }
}

// The stub extractor, shared by reindex() and the index AM's build: each
// token is an entity (exact-folded on its normalized surface), consecutive
// tokens in a row are a co-mention edge, and the source row is recorded as
// provenance. Sees every row, so it must run privileged (table owner /
// superuser / index build).
fn rebuild(watch_id: i32) -> i64 {
    use pgrx::Spi;
    let (source_table, text_column, pk_column) =
        Spi::get_three::<String, String, String>(&format!(
            "SELECT source_table::text, text_column, pk_column \
             FROM graphwright.watch WHERE id = {watch_id}"
        ))
        .expect("watch lookup");
    let source_table = source_table.expect("source table");
    let text_column = text_column.expect("text column");
    let pk_column = pk_column.expect("pk column");

    let rows: Vec<(String, String)> = Spi::connect(|client| {
        let sql = format!(
            "SELECT ({pk})::text, ({txt})::text FROM {tbl}",
            pk = ident(&pk_column),
            txt = ident(&text_column),
            tbl = source_table,
        );
        let table = client.select(&sql, None, &[])?;
        let mut out = Vec::new();
        for row in table {
            out.push((
                row.get::<String>(1)?.unwrap_or_default(),
                row.get::<String>(2)?.unwrap_or_default(),
            ));
        }
        Ok::<_, pgrx::spi::Error>(out)
    })
    .expect("read source rows");

    Spi::run(&format!(
        "DELETE FROM graphwright.entity WHERE watch_id = {watch_id}"
    ))
    .expect("clear watch");
    Spi::run(&format!(
        "DELETE FROM graphwright.dirty WHERE watch_id = {watch_id}"
    ))
    .expect("clear queue");

    let mut mentions = 0i64;
    for (pk, body) in &rows {
        mentions += index_row(watch_id, pk, body);
    }
    mentions
}

// Add one row's contribution from its source text (tokenize, then
// resolve). Used by the no-index reindex path.
fn index_row(watch_id: i32, source_pk: &str, body: &str) -> i64 {
    let relations = relation_extractor_set().then(|| run_relation_extractor(body));
    resolve_tokens(
        watch_id,
        source_pk,
        &extract(body),
        &std::collections::HashMap::new(),
        &read_overrides(watch_id),
        relations.as_deref(),
    )
}

// The canonical entity key for a surface at a row: normalize, fold through
// the cross-row merge map, then apply any per-mention override that forks
// this occurrence onto a private key. None when the surface normalizes away.
fn entity_key_for(
    surface: &str,
    source_pk: &str,
    canon: &std::collections::HashMap<String, String>,
    overrides: &std::collections::HashMap<(String, String), String>,
) -> Option<String> {
    let norm = normalize_name(surface);
    if norm.is_empty() {
        return None;
    }
    let canon_key = canon.get(&norm).cloned().unwrap_or_else(|| norm.clone());
    Some(match overrides.get(&(source_pk.to_string(), norm)) {
        Some(tag) => format!("{canon_key}{OVERRIDE_SEP}{tag}"),
        None => canon_key,
    })
}

// Upsert one edge and record the row as its provenance. predicate carries
// the relation type; co-mention edges use 'co_mentioned'.
fn insert_edge(watch_id: i32, src: i64, dst: i64, predicate: &str, source_pk: &str) {
    use pgrx::Spi;
    let edge_id = Spi::get_one::<i64>(&format!(
        "INSERT INTO graphwright.edge (watch_id, src, dst, predicate) \
         VALUES ({watch_id}, {src}, {dst}, {pred}) \
         ON CONFLICT (watch_id, src, dst, predicate) DO UPDATE SET predicate = EXCLUDED.predicate \
         RETURNING id",
        pred = lit(predicate),
    ))
    .expect("edge upsert")
    .expect("edge id");
    Spi::run(&format!(
        "INSERT INTO graphwright.edge_support (edge_id, watch_id, source_pk) \
         VALUES ({edge_id}, {watch_id}, {pk}) ON CONFLICT DO NOTHING",
        pk = lit(source_pk),
    ))
    .expect("edge support");
}

// Resolve a row's extracted tokens into entities and mentions, then build
// edges, tagged with the row's provenance. With `relations`, edges are the
// directed, typed triples; without, consecutive entities are co-mention.
fn resolve_tokens(
    watch_id: i32,
    source_pk: &str,
    tokens: &[String],
    canon: &std::collections::HashMap<String, String>,
    overrides: &std::collections::HashMap<(String, String), String>,
    relations: Option<&[(String, String, String)]>,
) -> i64 {
    use pgrx::Spi;
    let mut ids: Vec<i64> = Vec::new();
    let mut by_key: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut mentions = 0i64;
    for tok in tokens {
        let Some(entity_key) = entity_key_for(tok, source_pk, canon, overrides) else {
            continue;
        };
        let entity_id = Spi::get_one::<i64>(&format!(
            "INSERT INTO graphwright.entity (watch_id, surface, norm) VALUES ({watch_id}, {surf}, {norm}) \
             ON CONFLICT (watch_id, norm) DO UPDATE SET norm = EXCLUDED.norm \
             RETURNING id",
            surf = lit(tok),
            norm = lit(&entity_key),
        ))
        .expect("entity upsert")
        .expect("entity id");
        for key in phonetic_keys(tok) {
            Spi::run(&format!(
                "INSERT INTO graphwright.entity_phonetic (entity_id, key) VALUES ({entity_id}, {k}) \
                 ON CONFLICT DO NOTHING",
                k = lit(&key),
            ))
            .expect("phonetic key insert");
        }
        Spi::run(&format!(
            "INSERT INTO graphwright.mention (watch_id, entity_id, source_pk, surface_form) \
             VALUES ({watch_id}, {entity_id}, {pk}, {sf})",
            pk = lit(source_pk),
            sf = lit(tok),
        ))
        .expect("mention insert");
        mentions += 1;
        ids.push(entity_id);
        by_key.insert(entity_key, entity_id);
    }
    match relations {
        // Only relations whose subject and object were both extracted become
        // edges, so an edge never points at an entity with no mention behind
        // it (which the orphan cleanup would later drop).
        Some(triples) => {
            for (subject, predicate, object) in triples {
                let (Some(sk), Some(ok)) = (
                    entity_key_for(subject, source_pk, canon, overrides),
                    entity_key_for(object, source_pk, canon, overrides),
                ) else {
                    continue;
                };
                if let (Some(&s), Some(&o)) = (by_key.get(&sk), by_key.get(&ok)) {
                    if s != o {
                        insert_edge(watch_id, s, o, predicate, source_pk);
                    }
                }
            }
        }
        None => {
            for pair in ids.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if a == b {
                    continue;
                }
                let (src, dst) = if a < b { (a, b) } else { (b, a) };
                insert_edge(watch_id, src, dst, "co_mentioned", source_pk);
            }
        }
    }
    mentions
}

// Remove one row's contribution, dropping any entity or edge that loses
// its last support.
fn remove_row(watch_id: i32, source_pk: &str) {
    use pgrx::Spi;
    let p = lit(source_pk);
    Spi::run(&format!(
        "DELETE FROM graphwright.edge_support WHERE source_pk = {p} \
         AND edge_id IN (SELECT id FROM graphwright.edge WHERE watch_id = {watch_id})"
    ))
    .expect("drop edge support");
    Spi::run(&format!(
        "DELETE FROM graphwright.edge e WHERE e.watch_id = {watch_id} \
         AND NOT EXISTS (SELECT 1 FROM graphwright.edge_support s WHERE s.edge_id = e.id)"
    ))
    .expect("drop unsupported edges");
    Spi::run(&format!(
        "DELETE FROM graphwright.mention WHERE watch_id = {watch_id} AND source_pk = {p}"
    ))
    .expect("drop mentions");
    Spi::run(&format!(
        "DELETE FROM graphwright.entity en WHERE en.watch_id = {watch_id} \
         AND NOT EXISTS (SELECT 1 FROM graphwright.mention m WHERE m.entity_id = en.id)"
    ))
    .expect("drop orphan entities");
}

// Re-extract a single source row by its ctid, if it still exists.
fn add_row(watch_id: i32, source_pk: &str) {
    use pgrx::Spi;
    let (source_table, text_column, pk_column) = Spi::get_three::<String, String, String>(&format!(
        "SELECT source_table::text, text_column, pk_column FROM graphwright.watch WHERE id = {watch_id}"
    ))
    .expect("watch lookup");
    let source_table = source_table.expect("source table");
    let text_column = text_column.expect("text column");
    let pk_column = pk_column.expect("pk column");
    let body = Spi::get_one::<String>(&format!(
        "SELECT ({txt})::text FROM {tbl} WHERE ({pk})::text = {p}",
        txt = ident(&text_column),
        tbl = source_table,
        pk = ident(&pk_column),
        p = lit(source_pk),
    ))
    .expect("read row");
    if let Some(body) = body {
        index_row(watch_id, source_pk, &body);
    }
}

// Drain the change queue for a watch, applying each queued change. Each
// entry is removed first (idempotent), then re-added for an upsert.
fn process_dirty(watch_id: i32) -> i64 {
    use pgrx::Spi;
    let entries: Vec<(i64, String, String)> = Spi::connect(|client| {
        let table = client.select(
            &format!(
                "SELECT id, source_pk, op FROM graphwright.dirty \
                 WHERE watch_id = {watch_id} ORDER BY id"
            ),
            None,
            &[],
        )?;
        let mut out = Vec::new();
        for row in table {
            out.push((
                row.get::<i64>(1)?.expect("dirty id"),
                row.get::<String>(2)?.expect("source pk"),
                row.get::<String>(3)?.expect("op"),
            ));
        }
        Ok::<_, pgrx::spi::Error>(out)
    })
    .expect("read queue");

    if entries.is_empty() {
        return 0;
    }
    let max_id = entries.iter().map(|(id, _, _)| *id).max().unwrap_or(0);
    for (_, pk, op) in &entries {
        remove_row(watch_id, pk);
        if op == "upsert" {
            add_row(watch_id, pk);
        }
    }
    Spi::run(&format!(
        "DELETE FROM graphwright.dirty WHERE watch_id = {watch_id} AND id <= {max_id}"
    ))
    .expect("clear processed");
    entries.len() as i64
}

// ─── index storage ─────────────────────────────────────────────────
//
// A row's extraction (its tokens) is stored in the index relation's own
// pages, WAL-logged through generic WAL, so it is transactional with the
// heap and travels with physical replication, like pg_search. The cross-
// row resolved graph (entities/edges) is derived from this.
//
// Two record kinds, both keyed by heap ctid:
//   MARKER   [0][block u32][offset u16]                      "needs extraction"
//   MENTIONS [1][block u32][offset u16][n u16] then n*([len u16][utf8])
// aminsert writes a marker (fast); the async extract pass turns markers
// into mentions; resolution reads mentions.

pub const MARKER: u8 = 0;
pub const MENTIONS: u8 = 1;

// Separator joining a canonical norm to an override tag for a private
// entity key. ASCII Unit Separator never appears in a normalized name.
const OVERRIDE_SEP: char = '\u{1f}';

mod storage {
    use pgrx::pg_sys;

    const INVALID_OFFSET: u16 = 0;
    const FIRST_OFFSET: u16 = 1;

    pub fn marker(block: u32, offset: u16) -> Vec<u8> {
        let mut v = vec![super::MARKER];
        v.extend_from_slice(&block.to_le_bytes());
        v.extend_from_slice(&offset.to_le_bytes());
        v
    }

    pub fn mentions(block: u32, offset: u16, surfaces: &[String]) -> Vec<u8> {
        let mut v = vec![super::MENTIONS];
        v.extend_from_slice(&block.to_le_bytes());
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(&(surfaces.len() as u16).to_le_bytes());
        for s in surfaces {
            let b = s.as_bytes();
            v.extend_from_slice(&(b.len() as u16).to_le_bytes());
            v.extend_from_slice(b);
        }
        v
    }

    pub fn decode(data: &[u8]) -> (u8, u32, u16, Vec<String>) {
        // A record is tag(1) + block(4) + offset(2) = 7 bytes minimum. We
        // wrote it, so a short or truncated record means corruption; degrade
        // to an empty marker rather than panic while a scan holds a buffer
        // lock (a panic there becomes a PANIC, not a recoverable ERROR).
        if data.len() < 7 {
            return (super::MARKER, 0, 0, Vec::new());
        }
        let tag = data[0];
        let block = u32::from_le_bytes(data[1..5].try_into().unwrap());
        let offset = u16::from_le_bytes(data[5..7].try_into().unwrap());
        let mut surfaces = Vec::new();
        if tag == super::MENTIONS && data.len() >= 9 {
            let n = u16::from_le_bytes(data[7..9].try_into().unwrap()) as usize;
            let mut pos = 9;
            for _ in 0..n {
                if pos + 2 > data.len() {
                    break;
                }
                let len = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap()) as usize;
                pos += 2;
                if pos + len > data.len() {
                    break;
                }
                surfaces.push(String::from_utf8_lossy(&data[pos..pos + len]).into_owned());
                pos += len;
            }
        }
        (tag, block, offset, surfaces)
    }

    // Index pages are always shared buffers, so the page pointer is a
    // fixed offset into the shared block array (this is what BufferGetPage
    // expands to; pgrx does not wrap that inline macro).
    unsafe fn page(buffer: pg_sys::Buffer) -> pg_sys::Page {
        pg_sys::BufferBlocks.add((buffer as usize - 1) * pg_sys::BLCKSZ as usize) as pg_sys::Page
    }

    unsafe fn is_new(page: pg_sys::Page) -> bool {
        (*(page as *mut pg_sys::PageHeaderData)).pd_upper == 0
    }

    // Append a record, WAL-logged. Extends the relation when the last page
    // is full.
    pub unsafe fn append(indexrel: pg_sys::Relation, data: &[u8]) {
        let fork = pg_sys::ForkNumber::MAIN_FORKNUM;
        let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(indexrel, fork);
        // u32::MAX is P_NEW: extend the relation by a fresh page.
        let mut block = if nblocks == 0 { u32::MAX } else { nblocks - 1 };
        loop {
            let buf = pg_sys::ReadBufferExtended(
                indexrel,
                fork,
                block,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            );
            pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
            let state = pg_sys::GenericXLogStart(indexrel);
            let pg = pg_sys::GenericXLogRegisterBuffer(state, buf, 0);
            if is_new(pg) {
                pg_sys::PageInit(pg, pg_sys::BLCKSZ as usize, 0);
            }
            let off = pg_sys::PageAddItemExtended(
                pg,
                data.as_ptr() as pg_sys::Item,
                data.len(),
                INVALID_OFFSET,
                0,
            );
            if off == INVALID_OFFSET {
                pg_sys::GenericXLogAbort(state);
                pg_sys::UnlockReleaseBuffer(buf);
                if block == u32::MAX {
                    pgrx::error!("graphwright: a row's tokens do not fit on one page");
                }
                block = u32::MAX; // extend a fresh page and retry
                continue;
            }
            pg_sys::GenericXLogFinish(state);
            pg_sys::UnlockReleaseBuffer(buf);
            return;
        }
    }

    // Read every live record.
    pub unsafe fn scan(indexrel: pg_sys::Relation) -> Vec<Vec<u8>> {
        let fork = pg_sys::ForkNumber::MAIN_FORKNUM;
        let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(indexrel, fork);
        let mut out = Vec::new();
        for blk in 0..nblocks {
            let buf = pg_sys::ReadBufferExtended(
                indexrel,
                fork,
                blk,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            );
            pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
            let pg = page(buf);
            if !is_new(pg) {
                let maxoff = pg_sys::PageGetMaxOffsetNumber(pg);
                let mut off = FIRST_OFFSET;
                while off <= maxoff {
                    let iid = pg_sys::PageGetItemId(pg, off);
                    if (*iid).lp_flags() == pg_sys::LP_NORMAL {
                        let item = pg_sys::PageGetItem(pg, iid) as *const u8;
                        let len = (*iid).lp_len() as usize;
                        out.push(std::slice::from_raw_parts(item, len).to_vec());
                    }
                    off += 1;
                }
            }
            pg_sys::UnlockReleaseBuffer(buf);
        }
        out
    }

    // Mark dead every record whose heap ctid `is_dead` rejects, WAL-logged.
    // Returns the number removed. (LP_DEAD makes scan skip it, so the record
    // can no longer be resolved, even if a future row reuses that ctid.)
    pub unsafe fn prune(
        indexrel: pg_sys::Relation,
        is_dead: &mut dyn FnMut(u32, u16) -> bool,
    ) -> u64 {
        let fork = pg_sys::ForkNumber::MAIN_FORKNUM;
        let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(indexrel, fork);
        let mut removed = 0u64;
        for blk in 0..nblocks {
            let buf = pg_sys::ReadBufferExtended(
                indexrel,
                fork,
                blk,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            );
            pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
            let state = pg_sys::GenericXLogStart(indexrel);
            let pg = pg_sys::GenericXLogRegisterBuffer(state, buf, 0);
            let mut changed = false;
            if !is_new(pg) {
                let maxoff = pg_sys::PageGetMaxOffsetNumber(pg);
                let mut off = FIRST_OFFSET;
                while off <= maxoff {
                    let iid = pg_sys::PageGetItemId(pg, off);
                    if (*iid).lp_flags() == pg_sys::LP_NORMAL {
                        let item = pg_sys::PageGetItem(pg, iid) as *const u8;
                        let len = (*iid).lp_len() as usize;
                        let (_, block, offset, _) = decode(std::slice::from_raw_parts(item, len));
                        if is_dead(block, offset) {
                            (*iid).set_lp_flags(pg_sys::LP_DEAD);
                            (*iid).set_lp_off(0);
                            (*iid).set_lp_len(0);
                            removed += 1;
                            changed = true;
                        }
                    }
                    off += 1;
                }
            }
            if changed {
                pg_sys::GenericXLogFinish(state);
            } else {
                pg_sys::GenericXLogAbort(state);
            }
            pg_sys::UnlockReleaseBuffer(buf);
        }
        removed
    }
}

// Garbage-collect index storage: drop records whose heap row no longer
// exists. Must run privileged (it checks heap liveness; an RLS-limited
// view would wrongly prune rows it cannot see). ambulkdelete does the same
// during vacuum, driven by the vacuum callback instead of a heap probe.
fn gc(index: &str) -> i64 {
    use pgrx::Spi;
    let index_oid = Spi::get_one::<pg_sys::Oid>(&format!("SELECT {}::regclass::oid", lit(index)))
        .expect("index oid")
        .expect("index oid");
    let heap = Spi::get_one::<String>(&format!(
        "SELECT indrelid::regclass::text FROM pg_index WHERE indexrelid = {}::oid",
        index_oid.to_u32()
    ))
    .expect("heap lookup")
    .expect("heap");

    // Pass 1: collect stored ctids under a share lock only.
    let ctids: Vec<(u32, u16)> = unsafe {
        let rel = pg_sys::relation_open(index_oid, pg_sys::AccessShareLock as i32);
        let recs = storage::scan(rel);
        pg_sys::relation_close(rel, pg_sys::AccessShareLock as i32);
        recs.iter()
            .map(|r| {
                let (_, b, o, _) = storage::decode(r);
                (b, o)
            })
            .collect()
    };

    // Pass 2: which ctids no longer point at a live heap row? (No buffer
    // lock held while probing the heap.)
    let mut dead = std::collections::HashSet::new();
    for (b, o) in &ctids {
        let live = Spi::get_one::<bool>(&format!(
            "SELECT EXISTS (SELECT 1 FROM {heap} WHERE ctid = '({b},{o})'::tid)"
        ))
        .ok()
        .flatten()
        .unwrap_or(false);
        if !live {
            dead.insert((*b, *o));
        }
    }

    // Pass 3: prune them from storage.
    let removed = unsafe {
        let rel = pg_sys::relation_open(index_oid, pg_sys::RowExclusiveLock as i32);
        let mut pred = |b: u32, o: u16| dead.contains(&(b, o));
        let n = storage::prune(rel, &mut pred);
        pg_sys::relation_close(rel, pg_sys::RowExclusiveLock as i32);
        n
    };
    removed as i64
}

// Open an index by name, read back its stored per-row tokens. Lets a test
// confirm the data really lives in index storage.
fn index_dump(index: &str) -> Vec<(String, Vec<Option<String>>)> {
    let oid = pgrx::Spi::get_one::<pg_sys::Oid>(&format!("SELECT {}::regclass::oid", lit(index)))
        .expect("index oid")
        .expect("index oid");
    let mut out = Vec::new();
    unsafe {
        let rel = pg_sys::relation_open(oid, pg_sys::AccessShareLock as i32);
        for rec in storage::scan(rel) {
            let (tag, block, offset, surfaces) = storage::decode(&rec);
            if tag != MENTIONS {
                continue;
            }
            out.push((
                format!("({block},{offset})"),
                surfaces.into_iter().map(Some).collect(),
            ));
        }
        pg_sys::relation_close(rel, pg_sys::AccessShareLock as i32);
    }
    out
}

// ─── index access method ───────────────────────────────────────────
//
// `CREATE INDEX ... USING graphwright (body)` registers the table's text
// column as a watch (ctid provenance), writes each row's tokens into the
// index's own storage (the native, WAL-logged path), and builds the
// resolved graph in the catalog tables. The accessors query the graph and
// filter it by RLS. Incremental aminsert / ambulkdelete are no-ops for
// now; the change queue keeps the resolved graph current.

use core::ffi::{c_int, c_void};

struct BuildState {
    indexrel: pg_sys::Relation,
    ntuples: f64,
}

// Per-row build callback: write a marker for each row into index storage.
// ambuild then runs extract_pending to turn the markers into mentions.
#[pg_guard]
unsafe extern "C-unwind" fn build_callback(
    _index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    _values: *mut pg_sys::Datum,
    _isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut c_void,
) {
    let st = &mut *(state as *mut BuildState);
    let block = (((*tid).ip_blkid.bi_hi as u32) << 16) | (*tid).ip_blkid.bi_lo as u32;
    let offset = (*tid).ip_posid;
    storage::append(st.indexrel, &storage::marker(block, offset));
    st.ntuples += 1.0;
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambuild(
    heaprel: pg_sys::Relation,
    indexrel: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let attnum = (*index_info).ii_IndexAttrNumbers[0];
    if attnum == 0 {
        pgrx::error!("graphwright index requires a plain column, not an expression");
    }
    let heap_oid = (*heaprel).rd_id;
    let cname = pg_sys::get_attname(heap_oid, attnum, false);
    let column = std::ffi::CStr::from_ptr(cname)
        .to_string_lossy()
        .into_owned();
    let table =
        pgrx::Spi::get_one::<String>(&format!("SELECT {}::regclass::text", heap_oid.to_u32()))
            .expect("table name")
            .expect("table name");
    let _watch_id = upsert_watch(&table, &column, "ctid");

    // Mark every row in storage. Extraction and the resolved graph build on
    // the next maintain()/worker tick, which runs as the extension owner so
    // it sees every row (not just the index creator's RLS-visible ones) and
    // its writes bypass the catalog row-level security.
    let mut st = BuildState {
        indexrel,
        ntuples: 0.0,
    };
    pg_sys::table_index_build_scan(
        heaprel,
        indexrel,
        index_info,
        true,
        false,
        Some(build_callback),
        &mut st as *mut _ as *mut c_void,
        std::ptr::null_mut(),
    );

    let mut result = pgrx::PgBox::<pg_sys::IndexBuildResult>::alloc0();
    result.heap_tuples = st.ntuples;
    result.index_tuples = st.ntuples;
    result.into_pg()
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambuildempty(_indexrel: pg_sys::Relation) {}

// Mark the new row for extraction (a tiny WAL-logged write in the writing
// transaction). The extractor and the resolved graph catch up on the next
// maintain(), so a slow model never blocks the write.
#[pg_guard]
#[allow(clippy::too_many_arguments)] // the aminsert callback signature is fixed by Postgres
pub unsafe extern "C-unwind" fn aminsert(
    indexrel: pg_sys::Relation,
    _values: *mut pg_sys::Datum,
    _isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    _heaprel: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    // Just a marker; the (possibly slow) extraction runs later, off the
    // writing transaction.
    let block = (((*heap_tid).ip_blkid.bi_hi as u32) << 16) | (*heap_tid).ip_blkid.bi_lo as u32;
    let offset = (*heap_tid).ip_posid;
    storage::append(indexrel, &storage::marker(block, offset));
    false
}

// Vacuum is removing some heap tuples; drop their records from index
// storage so a reused ctid can never resolve against a stale record.
#[pg_guard]
pub unsafe extern "C-unwind" fn ambulkdelete(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    callback_state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let stats = if stats.is_null() {
        pgrx::PgBox::<pg_sys::IndexBulkDeleteResult>::alloc0().into_pg()
    } else {
        stats
    };
    if let Some(cb) = callback {
        let mut is_dead = |block: u32, offset: u16| -> bool {
            let mut tid = pg_sys::ItemPointerData {
                ip_blkid: pg_sys::BlockIdData {
                    bi_hi: (block >> 16) as u16,
                    bi_lo: (block & 0xffff) as u16,
                },
                ip_posid: offset,
            };
            cb(&mut tid as *mut _, callback_state)
        };
        let removed = storage::prune((*info).index, &mut is_dead);
        (*stats).tuples_removed += removed as f64;
    }
    stats
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amvacuumcleanup(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    stats
}

// No scans are served (amgettuple/amgetbitmap are None), so price the AM
// out of any scan path.
#[pg_guard]
#[allow(clippy::too_many_arguments)] // the amcostestimate callback signature is fixed by Postgres
pub unsafe extern "C-unwind" fn amcostestimate(
    _root: *mut pg_sys::PlannerInfo,
    _path: *mut pg_sys::IndexPath,
    _loop_count: f64,
    index_startup_cost: *mut pg_sys::Cost,
    index_total_cost: *mut pg_sys::Cost,
    index_selectivity: *mut pg_sys::Selectivity,
    index_correlation: *mut f64,
    index_pages: *mut f64,
) {
    *index_startup_cost = 0.0;
    *index_total_cost = f64::MAX;
    *index_selectivity = 1.0;
    *index_correlation = 0.0;
    *index_pages = 0.0;
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amoptions(
    _reloptions: pg_sys::Datum,
    _validate: bool,
) -> *mut pg_sys::bytea {
    std::ptr::null_mut()
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amvalidate(_opclassoid: pg_sys::Oid) -> bool {
    true
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambeginscan(
    indexrel: pg_sys::Relation,
    nkeys: c_int,
    norderbys: c_int,
) -> pg_sys::IndexScanDesc {
    pg_sys::RelationGetIndexScan(indexrel, nkeys, norderbys)
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amrescan(
    _scan: pg_sys::IndexScanDesc,
    _keys: pg_sys::ScanKey,
    _nkeys: c_int,
    _orderbys: pg_sys::ScanKey,
    _norderbys: c_int,
) {
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amendscan(_scan: pg_sys::IndexScanDesc) {}

#[pg_extern(sql = "
CREATE FUNCTION graphwright_amhandler(internal) RETURNS index_am_handler
    PARALLEL SAFE IMMUTABLE STRICT COST 0.0001
    LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
CREATE ACCESS METHOD graphwright TYPE INDEX HANDLER graphwright_amhandler;
CREATE OPERATOR CLASS graphwright_text_ops DEFAULT FOR TYPE text
    USING graphwright AS STORAGE text;
")]
fn graphwright_amhandler(_fcinfo: pg_sys::FunctionCallInfo) -> pgrx::PgBox<pg_sys::IndexAmRoutine> {
    let mut amroutine = unsafe {
        pgrx::PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine)
    };

    amroutine.amstrategies = 0;
    amroutine.amsupport = 0;
    amroutine.amcanmulticol = false;
    amroutine.amkeytype = pg_sys::InvalidOid;

    amroutine.ambuild = Some(ambuild);
    amroutine.ambuildempty = Some(ambuildempty);
    amroutine.aminsert = Some(aminsert);
    amroutine.ambulkdelete = Some(ambulkdelete);
    amroutine.amvacuumcleanup = Some(amvacuumcleanup);
    amroutine.amcostestimate = Some(amcostestimate);
    amroutine.amoptions = Some(amoptions);
    amroutine.amvalidate = Some(amvalidate);
    amroutine.ambeginscan = Some(ambeginscan);
    amroutine.amrescan = Some(amrescan);
    amroutine.amendscan = Some(amendscan);
    amroutine.amgettuple = None;
    amroutine.amgetbitmap = None;

    amroutine.into_pg_boxed()
}

// ─── background maintenance worker ──────────────────────────────────
//
// Drains the change queue for every watch on an interval, so the graph
// stays current without anyone calling process_dirty. It connects to one
// database (the graphwright.database GUC), so it needs that set plus
// shared_preload_libraries = 'pg_graphwright'. graphwright.maintain()
// runs the same drain on demand (e.g. from pg_cron) without the worker.

use pgrx::bgworkers::{
    BackgroundWorker, BackgroundWorkerBuilder, BgWorkerStartTime, SignalWakeFlags,
};
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use std::ffi::CString;
use std::time::Duration;

static MAINTENANCE_DATABASE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
static EXTRACTOR: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
static JUDGE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
static RELATION_EXTRACTOR: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
static EMBEDDER: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
static EMBEDDING_THRESHOLD: GucSetting<f64> = GucSetting::<f64>::new(0.83);

// Bring every graphwright graph current: re-resolve each index from its
// own storage (the index path), and drain the change queue (the no-index
// reindex path). Returns a rough count of watches touched.
fn drain_all() -> i64 {
    use pgrx::Spi;
    if Spi::get_one::<bool>("SELECT to_regnamespace('graphwright') IS NOT NULL")
        .ok()
        .flatten()
        != Some(true)
    {
        return 0;
    }
    let mut touched = 0i64;

    // Index path: re-resolve every graphwright index from its storage.
    let indexes: Vec<(pg_sys::Oid, pg_sys::Oid)> = Spi::connect(|client| {
        let table = client.select(
            "SELECT i.indexrelid, i.indrelid FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_am a ON a.oid = c.relam WHERE a.amname = 'graphwright'",
            None,
            &[],
        )?;
        let mut v = Vec::new();
        for row in table {
            v.push((
                row.get::<pg_sys::Oid>(1)?.expect("indexrelid"),
                row.get::<pg_sys::Oid>(2)?.expect("indrelid"),
            ));
        }
        Ok::<_, pgrx::spi::Error>(v)
    })
    .unwrap_or_default();
    for (indexrelid, heaprelid) in indexes {
        let watch_id = Spi::get_one::<i32>(&format!(
            "SELECT id FROM graphwright.watch WHERE source_table::oid = {}::oid",
            heaprelid.to_u32()
        ))
        .ok()
        .flatten();
        if let Some(wid) = watch_id {
            unsafe {
                let rel = pg_sys::relation_open(indexrelid, pg_sys::RowExclusiveLock as i32);
                extract_pending(rel, wid);
                resolve_from_storage(rel, wid);
                pg_sys::relation_close(rel, pg_sys::RowExclusiveLock as i32);
            }
            touched += 1;
        }
    }

    // No-index path: drain any queued source-row changes.
    let dirty: Vec<i32> = Spi::connect(|client| {
        let table = client.select("SELECT DISTINCT watch_id FROM graphwright.dirty", None, &[])?;
        let mut v = Vec::new();
        for row in table {
            v.push(row.get::<i32>(1)?.expect("watch id"));
        }
        Ok::<_, pgrx::spi::Error>(v)
    })
    .unwrap_or_default();
    touched += dirty.into_iter().map(process_dirty).sum::<i64>();
    touched
}

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    GucRegistry::define_string_guc(
        c"graphwright.database",
        c"Database the maintenance worker keeps current.",
        c"Empty disables the worker. Also needs shared_preload_libraries = 'pg_graphwright'.",
        &MAINTENANCE_DATABASE,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"graphwright.extractor",
        c"SQL function f(text) -> text[] used to extract entity surfaces.",
        c"Empty uses the built-in tokenizer. Called asynchronously by the maintenance pass.",
        &EXTRACTOR,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"graphwright.judge",
        c"SQL function j(text, text[]) -> text[] that validates the extractor output.",
        c"Empty applies no judge. A larger model can drop or keep mentions here.",
        &JUDGE,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"graphwright.relation_extractor",
        c"SQL function f(text) -> text[] of flattened (subject, predicate, object) triples.",
        c"Empty falls back to undirected co-mention edges. Set it for typed, directed edges.",
        &RELATION_EXTRACTOR,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"graphwright.embedder",
        c"SQL function f(text) -> float8[] that embeds a name for semantic matching.",
        c"Empty disables the embedding lane. Names whose vectors are close auto-merge.",
        &EMBEDDER,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_float_guc(
        c"graphwright.embedding_threshold",
        c"Cosine at or above which two embedded names auto-merge.",
        c"Default 0.83. Higher is stricter. Only used when graphwright.embedder is set.",
        &EMBEDDING_THRESHOLD,
        0.0,
        1.0,
        GucContext::Userset,
        GucFlags::default(),
    );
    // Registering a background worker is only valid during preload.
    if unsafe { !pg_sys::process_shared_preload_libraries_in_progress } {
        return;
    }
    BackgroundWorkerBuilder::new("graphwright maintenance")
        .set_function("graphwright_maintenance_worker")
        .set_library("pg_graphwright")
        .enable_spi_access()
        .set_start_time(BgWorkerStartTime::RecoveryFinished)
        .load();
}

#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn graphwright_maintenance_worker(_arg: pg_sys::Datum) {
    let database = match MAINTENANCE_DATABASE.get() {
        Some(d) if !d.to_string_lossy().is_empty() => d.to_string_lossy().into_owned(),
        _ => return, // no database configured; nothing to maintain
    };
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    BackgroundWorker::connect_worker_to_spi(Some(&database), None);
    while BackgroundWorker::wait_latch(Some(Duration::from_secs(10))) {
        if BackgroundWorker::sigterm_received() {
            break;
        }
        BackgroundWorker::transaction(|| {
            drain_all();
        });
    }
}

#[pg_schema]
mod graphwright {
    use super::{drain_all, ident, watch_meta, Visibility};
    use pgrx::prelude::*;

    // Catalog lives inside the schema module so pgrx orders it after the
    // schema is created.
    extension_sql!(
        r#"
CREATE TABLE graphwright.watch (
    id           serial PRIMARY KEY,
    source_table regclass NOT NULL,
    text_column  text NOT NULL,
    pk_column    text NOT NULL,
    visibility   text CHECK (visibility IN ('union', 'intersection')),
    UNIQUE (source_table, text_column)
);

CREATE TABLE graphwright.entity (
    id       bigserial PRIMARY KEY,
    watch_id integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    surface  text NOT NULL,
    norm     text NOT NULL,
    UNIQUE (watch_id, norm)
);

-- An entity's phonetic keys (cross-script consonant skeletons). Two
-- entities that share a key are a merge proposal, not an auto-merge.
CREATE TABLE graphwright.entity_phonetic (
    entity_id bigint NOT NULL REFERENCES graphwright.entity(id) ON DELETE CASCADE,
    key       text NOT NULL,
    PRIMARY KEY (entity_id, key)
);

-- Durable, human-owned decisions, replayed on every re-resolve: 'merge'
-- forces two norms to one entity, 'split' keeps them apart (vetoing a
-- phonetic auto-merge). Edit or delete a row to reverse the decision.
CREATE TABLE graphwright.decision (
    watch_id integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    norm_a   text NOT NULL,
    norm_b   text NOT NULL,
    verdict  text NOT NULL CHECK (verdict IN ('merge', 'split')),
    decided_by text NOT NULL DEFAULT current_user,
    decided_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (watch_id, norm_a, norm_b),
    -- The pair is ordered in Rust by codepoint (byte order for UTF-8); the
    -- check must use the same order, not the database collation, or a
    -- cross-script pair like (ali, علی) fails it under a non-C collation.
    CHECK (norm_a < norm_b COLLATE "C")
);

CREATE TABLE graphwright.mention (
    id           bigserial PRIMARY KEY,
    watch_id     integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    entity_id    bigint NOT NULL REFERENCES graphwright.entity(id) ON DELETE CASCADE,
    source_pk    text NOT NULL,
    surface_form text NOT NULL
);

-- Per-mention identity override: pins one surface occurrence in one row to
-- a private entity, even when it normalizes to a shared key. This is how a
-- human separates two identical spellings the exact stage folded into one.
-- Delete the row to fold them back. The tag groups occurrences that should
-- share the same private entity (default: the row, so each split is its own).
CREATE TABLE graphwright.mention_override (
    watch_id     integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    source_pk    text NOT NULL,
    surface_norm text NOT NULL,
    tag          text NOT NULL,
    decided_by   text NOT NULL DEFAULT current_user,
    decided_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (watch_id, source_pk, surface_norm)
);

CREATE TABLE graphwright.edge (
    id        bigserial PRIMARY KEY,
    watch_id  integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    src       bigint NOT NULL REFERENCES graphwright.entity(id) ON DELETE CASCADE,
    dst       bigint NOT NULL REFERENCES graphwright.entity(id) ON DELETE CASCADE,
    predicate text NOT NULL DEFAULT 'co_mentioned',
    UNIQUE (watch_id, src, dst, predicate)
);

CREATE TABLE graphwright.edge_support (
    edge_id   bigint NOT NULL REFERENCES graphwright.edge(id) ON DELETE CASCADE,
    -- Denormalized from the edge so the row-level-security policy is self
    -- contained: a policy that read graphwright.edge would recurse, since
    -- edge's own policy reads edge_support.
    watch_id  integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    source_pk text NOT NULL,
    PRIMARY KEY (edge_id, source_pk)
);

-- Change queue: the capture trigger records which source rows are dirty
-- (ctid + op); process_dirty drains it and applies the changes.
CREATE TABLE graphwright.dirty (
    id        bigserial PRIMARY KEY,
    watch_id  integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    source_pk text NOT NULL,
    op        text NOT NULL CHECK (op IN ('upsert', 'delete'))
);

-- The capture trigger. SECURITY DEFINER so a writer who lacks rights on
-- the queue can still enqueue. search_path is pinned and every name is
-- schema-qualified, so a caller cannot shadow a referenced object via
-- pg_temp.
CREATE FUNCTION graphwright._enqueue() RETURNS trigger
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $enqueue$
BEGIN
    -- Provenance is the watch's pk column. The index path uses 'ctid' (a
    -- system column, absent from row_to_json), so it is read directly.
    IF TG_OP <> 'DELETE' THEN
        INSERT INTO graphwright.dirty (watch_id, source_pk, op)
        SELECT w.id,
               CASE WHEN w.pk_column = 'ctid' THEN NEW.ctid::text
                    ELSE row_to_json(NEW) ->> w.pk_column END,
               'upsert'
        FROM graphwright.watch w WHERE w.source_table = TG_RELID;
    END IF;
    IF TG_OP <> 'INSERT' THEN
        INSERT INTO graphwright.dirty (watch_id, source_pk, op)
        SELECT w.id,
               CASE WHEN w.pk_column = 'ctid' THEN OLD.ctid::text
                    ELSE row_to_json(OLD) ->> w.pk_column END,
               'delete'
        FROM graphwright.watch w WHERE w.source_table = TG_RELID;
    END IF;
    RETURN NULL;
END;
$enqueue$;

GRANT USAGE ON SCHEMA graphwright TO PUBLIC;
GRANT SELECT ON ALL TABLES IN SCHEMA graphwright TO PUBLIC;

-- Is source row `pk` visible to the current caller? SECURITY INVOKER, so
-- the probe runs the source table's row-level security as the caller. This
-- is the bridge that carries the source's RLS onto the derived graph.
-- search_path is intentionally NOT pinned: this runs the caller's own
-- source-table policy, which may reference the caller's unqualified objects
-- (helper functions, share tables). As an INVOKER function it runs with the
-- caller's privileges, so a pinned path would buy no safety and would break
-- legitimate policies. The body itself only touches schema-qualified names.
CREATE FUNCTION graphwright._pk_visible(wid integer, pk text) RETURNS boolean
    LANGUAGE plpgsql STABLE SECURITY INVOKER AS $pkv$
DECLARE
    tbl text;
    col text;
    ok  boolean;
BEGIN
    SELECT source_table::text, pk_column INTO tbl, col
    FROM graphwright.watch WHERE id = wid;
    IF tbl IS NULL THEN
        RETURN false;
    END IF;
    EXECUTE format('SELECT EXISTS (SELECT 1 FROM %s s WHERE (s.%I)::text = $1)', tbl, col)
        INTO ok USING pk;
    RETURN ok;
END;
$pkv$;

-- Every source_pk supporting an edge, read as the owner so it bypasses
-- edge_support's own row security. The edge policy applies _pk_visible to
-- these itself; reading edge_support directly would instead be filtered to
-- the visible supports and lose the ones intersection needs to count.
CREATE FUNCTION graphwright._edge_supports(eid bigint) RETURNS SETOF text
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $es$
    SELECT source_pk FROM graphwright.edge_support WHERE edge_id = eid
$es$;

-- The lockdown: row-level security ON the catalog content tables, so a
-- graph row is visible exactly when the source row(s) behind it are. The
-- accessors are SECURITY INVOKER and these policies filter them; a direct
-- SELECT on the catalog is filtered the same way, so the accessors are no
-- privileged back door. Maintenance runs as the owner, which bypasses RLS
-- (it is enabled, not forced), so the graph is still built over every row.
-- Each policy decides visibility through _pk_visible directly. A policy
-- cannot lean on another catalog table's RLS: Postgres does not re-apply
-- row security to tables read inside a policy expression, so the visibility
-- test must be self-contained here.
ALTER TABLE graphwright.mention ENABLE ROW LEVEL SECURITY;
CREATE POLICY mention_visible ON graphwright.mention
    USING (graphwright._pk_visible(watch_id, source_pk));

ALTER TABLE graphwright.entity ENABLE ROW LEVEL SECURITY;
CREATE POLICY entity_visible ON graphwright.entity
    USING (EXISTS (
        SELECT 1 FROM graphwright.mention mn
        WHERE mn.entity_id = entity.id
          AND graphwright._pk_visible(mn.watch_id, mn.source_pk)));

ALTER TABLE graphwright.entity_phonetic ENABLE ROW LEVEL SECURITY;
CREATE POLICY entity_phonetic_visible ON graphwright.entity_phonetic
    USING (EXISTS (
        SELECT 1 FROM graphwright.mention mn
        WHERE mn.entity_id = entity_phonetic.entity_id
          AND graphwright._pk_visible(mn.watch_id, mn.source_pk)));

-- A per-mention override carries the row it pins; a decision is visible when
-- one of its norms names an entity the caller can see. Both reveal names, so
-- both are filtered like the graph itself.
ALTER TABLE graphwright.mention_override ENABLE ROW LEVEL SECURITY;
CREATE POLICY mention_override_visible ON graphwright.mention_override
    USING (graphwright._pk_visible(watch_id, source_pk));

ALTER TABLE graphwright.decision ENABLE ROW LEVEL SECURITY;
CREATE POLICY decision_visible ON graphwright.decision
    USING (EXISTS (
        SELECT 1 FROM graphwright.entity e
        JOIN graphwright.mention mn ON mn.entity_id = e.id
        WHERE e.norm IN (decision.norm_a, decision.norm_b)
          AND graphwright._pk_visible(mn.watch_id, mn.source_pk)));

-- Edge visibility follows the watch's rule over its supporting rows:
-- union (any visible) or intersection (all visible). It reads the supports
-- through _edge_supports (owner-side) so it always sees every support, then
-- applies _pk_visible as the caller.
ALTER TABLE graphwright.edge ENABLE ROW LEVEL SECURITY;
CREATE POLICY edge_visible ON graphwright.edge
    USING (
        CASE WHEN COALESCE(
                 (SELECT visibility FROM graphwright.watch WHERE id = watch_id), 'union'
             ) = 'intersection'
        THEN NOT EXISTS (
            SELECT 1 FROM graphwright._edge_supports(edge.id) AS sp
            WHERE NOT graphwright._pk_visible(edge.watch_id, sp))
        ELSE EXISTS (
            SELECT 1 FROM graphwright._edge_supports(edge.id) AS sp
            WHERE graphwright._pk_visible(edge.watch_id, sp))
        END
    );

-- A support row is visible when its own source row is. It uses its own
-- watch_id (not a join to edge) so this policy and edge's policy do not
-- reference each other and recurse.
ALTER TABLE graphwright.edge_support ENABLE ROW LEVEL SECURITY;
CREATE POLICY edge_support_visible ON graphwright.edge_support
    USING (graphwright._pk_visible(watch_id, source_pk));
"#,
        name = "catalog",
    );

    // The maintenance and review functions run as the owner, so a plain
    // caller must not invoke them: maintenance/gc would let anyone trigger
    // owner-context work, the review functions would let anyone rewrite the
    // shared graph's identity, index_dump would return raw stored surfaces
    // past row-level security, and watch installs a capture trigger on a
    // named table and seeds extension state. Grant these to your operator
    // and reviewer roles. The read accessors stay open; the catalog RLS
    // filters them. Runs last (finalize) so the functions already exist.
    extension_sql!(
        r#"
REVOKE EXECUTE ON FUNCTION graphwright.watch(text, text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.reindex(integer) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.process_dirty(integer) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.maintain() FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.index_dump(text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.gc(text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.merge(text, text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.split(text, text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.unmerge(text, text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.split_mention(text, text, text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION graphwright.unsplit_mention(text, text, text) FROM PUBLIC;
"#,
        name = "lockdown_exec",
        finalize,
    );

    // Register a table's text column as a document source (no index).
    // Installs the capture trigger so changes queue for process_dirty.
    // pk_column names the column used as provenance back to the source row.
    #[pg_extern]
    fn watch(source_table: &str, text_column: &str, pk_column: &str) -> i32 {
        let id = super::upsert_watch(source_table, text_column, pk_column);
        super::install_capture_trigger(id);
        id
    }

    // Maintenance runs as the extension owner (SECURITY DEFINER): it must
    // see every source row and write the catalog past row-level security,
    // which the owner bypasses. It never SET ROLEs, so this is permitted.
    //
    // Rebuild the whole graph for a watch from the current source rows.
    // Shares the extraction core with the index AM's build path.
    #[pg_extern(security_definer)]
    fn reindex(watch_id: i32) -> i64 {
        super::rebuild(watch_id)
    }

    // Apply queued row changes to the graph. A background worker calls
    // this on an interval; you can also call it directly. Returns the
    // number of queued changes applied.
    #[pg_extern(security_definer)]
    fn process_dirty(watch_id: i32) -> i64 {
        super::process_dirty(watch_id)
    }

    // Drain the change queue for every watch (what the background worker
    // does each tick). Returns the number of changes applied. Call it
    // from pg_cron, or let the worker call it on an interval.
    #[pg_extern(security_definer)]
    fn maintain() -> i64 {
        drain_all()
    }

    // Read a graphwright index's per-row tokens back from its own storage.
    // Diagnostic: confirms the extraction lives in index pages.
    #[pg_extern(security_definer)]
    fn index_dump(
        index: &str,
    ) -> TableIterator<'static, (name!(ctid, String), name!(tokens, Vec<Option<String>>))> {
        TableIterator::new(super::index_dump(index))
    }

    // Reclaim storage records for rows that no longer exist (vacuum does
    // this automatically via ambulkdelete). Returns the number removed.
    #[pg_extern(security_definer)]
    fn gc(index: &str) -> i64 {
        super::gc(index)
    }

    // Entities visible to the caller: those mentioned by at least one
    // source row the caller can read. The EXISTS probe joins the source
    // table, so RLS filters it.
    #[pg_extern]
    fn entities(
        source_table: &str,
    ) -> TableIterator<'static, (name!(entity_id, i64), name!(surface, String))> {
        let m = watch_meta(source_table);
        let sql = format!(
            "SELECT e.id, e.surface FROM graphwright.entity e \
             WHERE e.watch_id = {wid} AND EXISTS ( \
               SELECT 1 FROM graphwright.mention mn \
               JOIN {tbl} s ON (s.{pk})::text = mn.source_pk \
               WHERE mn.entity_id = e.id) \
             ORDER BY e.surface",
            wid = m.id,
            tbl = m.source_table,
            pk = ident(&m.pk_column),
        );
        let rows: Vec<(i64, String)> = Spi::connect(|client| {
            let table = client.select(&sql, None, &[])?;
            let mut out = Vec::new();
            for row in table {
                out.push((
                    row.get::<i64>(1)?.expect("entity id"),
                    row.get::<String>(2)?.expect("surface"),
                ));
            }
            Ok::<_, pgrx::spi::Error>(out)
        })
        .expect("entities query");
        TableIterator::new(rows)
    }

    // Merge proposals: pairs of visible entities that share a phonetic key
    // (cross-script or spelling variants) but did not fold at the exact
    // stage. These are candidates for review, never auto-merged. Both
    // entities must be visible to the caller, so the RLS probe runs twice.
    #[pg_extern]
    fn proposals(
        source_table: &str,
    ) -> TableIterator<
        'static,
        (
            name!(entity_a, i64),
            name!(surface_a, String),
            name!(entity_b, i64),
            name!(surface_b, String),
        ),
    > {
        let m = watch_meta(source_table);
        let visible = |alias: &str| {
            format!(
                "EXISTS (SELECT 1 FROM graphwright.mention mn \
                 JOIN {tbl} s ON (s.{pk})::text = mn.source_pk WHERE mn.entity_id = {alias}.id)",
                tbl = m.source_table,
                pk = ident(&m.pk_column),
            )
        };
        let sql = format!(
            "SELECT e1.id, e1.surface, e2.id, e2.surface \
             FROM graphwright.entity_phonetic p1 \
             JOIN graphwright.entity_phonetic p2 ON p1.key = p2.key AND p1.entity_id < p2.entity_id \
             JOIN graphwright.entity e1 ON e1.id = p1.entity_id \
             JOIN graphwright.entity e2 ON e2.id = p2.entity_id \
             WHERE e1.watch_id = {wid} AND e2.watch_id = {wid} AND e1.norm <> e2.norm \
               AND {vis_a} AND {vis_b} \
             GROUP BY e1.id, e1.surface, e2.id, e2.surface \
             ORDER BY e1.surface, e2.surface",
            wid = m.id,
            vis_a = visible("e1"),
            vis_b = visible("e2"),
        );
        let rows: Vec<(i64, String, i64, String)> = Spi::connect(|client| {
            let table = client.select(&sql, None, &[])?;
            let mut out = Vec::new();
            for row in table {
                out.push((
                    row.get::<i64>(1)?.expect("entity a"),
                    row.get::<String>(2)?.expect("surface a"),
                    row.get::<i64>(3)?.expect("entity b"),
                    row.get::<String>(4)?.expect("surface b"),
                ));
            }
            Ok::<_, pgrx::spi::Error>(out)
        })
        .expect("proposals query");
        TableIterator::new(rows)
    }

    // Durable, reversible identity decisions. merge() forces two names to
    // one entity; split() keeps them apart (vetoing a phonetic auto-merge);
    // unmerge() drops the decision. Each re-resolves immediately and is
    // replayed on every later re-resolve. decisions() lists them. These
    // re-resolve the graph, so they run as the owner like maintain().
    #[pg_extern(security_definer)]
    fn merge(source_table: &str, a: &str, b: &str) -> bool {
        super::record_decision(source_table, a, b, "merge")
    }

    #[pg_extern(security_definer)]
    fn split(source_table: &str, a: &str, b: &str) -> bool {
        super::record_decision(source_table, a, b, "split")
    }

    #[pg_extern(security_definer)]
    fn unmerge(source_table: &str, a: &str, b: &str) -> bool {
        super::drop_decision(source_table, a, b)
    }

    #[pg_extern]
    fn decisions(
        source_table: &str,
    ) -> TableIterator<
        'static,
        (
            name!(norm_a, String),
            name!(norm_b, String),
            name!(verdict, String),
        ),
    > {
        TableIterator::new(super::list_decisions(source_table))
    }

    // Per-mention identity override. split_mention pins one surface
    // occurrence in one row (source_pk, e.g. its ctid) to a private entity,
    // separating two identical spellings the exact stage folded. tag groups
    // splits that should share one private entity (NULL: the row stands
    // alone). unsplit_mention drops it, folding them back. Both re-resolve.
    #[pg_extern(security_definer)]
    fn split_mention(
        source_table: &str,
        source_pk: &str,
        surface: &str,
        tag: default!(Option<&str>, "NULL"),
    ) -> bool {
        super::record_mention_override(source_table, source_pk, surface, tag.unwrap_or(""))
    }

    #[pg_extern(security_definer)]
    fn unsplit_mention(source_table: &str, source_pk: &str, surface: &str) -> bool {
        super::drop_mention_override(source_table, source_pk, surface)
    }

    // Mentions visible to the caller: the raw occurrences behind the graph,
    // with the source row's pk (for split_mention) and resolved entity. The
    // join to the source table runs as the caller, so RLS filters it.
    #[pg_extern]
    fn mentions(
        source_table: &str,
    ) -> TableIterator<
        'static,
        (
            name!(entity_id, i64),
            name!(surface, String),
            name!(source_pk, String),
            name!(surface_form, String),
        ),
    > {
        let m = watch_meta(source_table);
        let sql = format!(
            "SELECT mn.entity_id, e.surface, mn.source_pk, mn.surface_form \
             FROM graphwright.mention mn \
             JOIN graphwright.entity e ON e.id = mn.entity_id \
             JOIN {tbl} s ON (s.{pk})::text = mn.source_pk \
             WHERE mn.watch_id = {wid} \
             ORDER BY e.surface, mn.source_pk, mn.surface_form",
            wid = m.id,
            tbl = m.source_table,
            pk = ident(&m.pk_column),
        );
        let rows: Vec<(i64, String, String, String)> = Spi::connect(|client| {
            let table = client.select(&sql, None, &[])?;
            let mut out = Vec::new();
            for row in table {
                out.push((
                    row.get::<i64>(1)?.expect("entity id"),
                    row.get::<String>(2)?.expect("surface"),
                    row.get::<String>(3)?.expect("source_pk"),
                    row.get::<String>(4)?.expect("surface_form"),
                ));
            }
            Ok::<_, pgrx::spi::Error>(out)
        })
        .expect("mentions query");
        TableIterator::new(rows)
    }

    // Edges visible to the caller. Visibility derives from the supporting
    // rows per the watch's rule: union (any supporting row visible) or
    // intersection (all of them). The join to the source table is what
    // applies RLS.
    #[pg_extern]
    fn edges(
        source_table: &str,
    ) -> TableIterator<
        'static,
        (
            name!(edge_id, i64),
            name!(src, String),
            name!(dst, String),
            name!(predicate, String),
        ),
    > {
        let m = watch_meta(source_table);
        let visible = match m.visibility {
            Visibility::Union => format!(
                "EXISTS (SELECT 1 FROM graphwright.edge_support sup \
                 JOIN {tbl} s ON (s.{pk})::text = sup.source_pk WHERE sup.edge_id = e.id)",
                tbl = m.source_table,
                pk = ident(&m.pk_column),
            ),
            Visibility::Intersection => format!(
                "(SELECT count(*) FROM graphwright.edge_support sup \
                  JOIN {tbl} s ON (s.{pk})::text = sup.source_pk WHERE sup.edge_id = e.id) \
                 = (SELECT count(*) FROM graphwright.edge_support sup WHERE sup.edge_id = e.id)",
                tbl = m.source_table,
                pk = ident(&m.pk_column),
            ),
        };
        let sql = format!(
            "SELECT e.id, es.surface, ed.surface, e.predicate \
             FROM graphwright.edge e \
             JOIN graphwright.entity es ON es.id = e.src \
             JOIN graphwright.entity ed ON ed.id = e.dst \
             WHERE e.watch_id = {wid} AND {visible} \
             ORDER BY es.surface, ed.surface",
            wid = m.id,
        );
        let rows: Vec<(i64, String, String, String)> = Spi::connect(|client| {
            let table = client.select(&sql, None, &[])?;
            let mut out = Vec::new();
            for row in table {
                out.push((
                    row.get::<i64>(1)?.expect("edge id"),
                    row.get::<String>(2)?.expect("src"),
                    row.get::<String>(3)?.expect("dst"),
                    row.get::<String>(4)?.expect("predicate"),
                ));
            }
            Ok::<_, pgrx::spi::Error>(out)
        })
        .expect("edges query");
        TableIterator::new(rows)
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    // Three roles' worth of notes behind an RLS policy, then the graph
    // built over all of them as superuser.
    fn setup() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("ALTER TABLE notes ENABLE ROW LEVEL SECURITY").unwrap();
        Spi::run("CREATE POLICY owner_can_read ON notes USING (owner = current_user)").unwrap();
        Spi::run("GRANT SELECT ON notes TO PUBLIC").unwrap();
        Spi::run("CREATE ROLE role_a").unwrap();
        Spi::run("CREATE ROLE role_b").unwrap();
        Spi::run(
            "INSERT INTO notes VALUES \
             (1, 'role_a', 'Sara Tehran'), \
             (2, 'role_b', 'Sara Berlin'), \
             (3, 'role_a', 'Reza Berlin'), \
             (4, 'role_a', 'Sara Berlin')",
        )
        .unwrap();
        let wid = Spi::get_one::<i32>("SELECT graphwright.watch('notes', 'body', 'id')")
            .unwrap()
            .unwrap();
        Spi::run(&format!("SELECT graphwright.reindex({wid})")).unwrap();
    }

    fn edges_as(role: &str) -> Vec<(String, String)> {
        Spi::run(&format!("SET ROLE {role}")).unwrap();
        let out = Spi::connect(|client| {
            let table = client.select(
                "SELECT src, dst FROM graphwright.edges('notes') ORDER BY src, dst",
                None,
                &[],
            )?;
            let mut v = Vec::new();
            for row in table {
                v.push((
                    row.get::<String>(1)?.unwrap(),
                    row.get::<String>(2)?.unwrap(),
                ));
            }
            Ok::<_, pgrx::spi::Error>(v)
        })
        .unwrap();
        Spi::run("RESET ROLE").unwrap();
        out
    }

    fn entities_as(role: &str) -> Vec<String> {
        Spi::run(&format!("SET ROLE {role}")).unwrap();
        let out = Spi::connect(|client| {
            let table = client.select(
                "SELECT surface FROM graphwright.entities('notes') ORDER BY surface",
                None,
                &[],
            )?;
            let mut v = Vec::new();
            for row in table {
                v.push(row.get::<String>(1)?.unwrap());
            }
            Ok::<_, pgrx::spi::Error>(v)
        })
        .unwrap();
        Spi::run("RESET ROLE").unwrap();
        out
    }

    #[pg_test]
    fn entities_follow_row_visibility() {
        setup();
        // role_a reads rows 1,3,4 -> sara, tehran, reza, berlin
        assert_eq!(
            entities_as("role_a"),
            vec!["berlin", "reza", "sara", "tehran"]
        );
        // role_b reads row 2 -> sara, berlin
        assert_eq!(entities_as("role_b"), vec!["berlin", "sara"]);
    }

    #[pg_test]
    fn union_shows_edges_with_any_visible_support() {
        setup(); // visibility NULL -> union
        let a = edges_as("role_a");
        assert!(a.contains(&("sara".into(), "berlin".into()))); // via row 4
        assert!(a.contains(&("sara".into(), "tehran".into())));
        assert!(a.contains(&("berlin".into(), "reza".into())));
        // role_b sees only the sara-berlin edge, via row 2
        assert_eq!(edges_as("role_b"), vec![("sara".into(), "berlin".into())]);
    }

    #[pg_test]
    fn intersection_hides_partially_visible_edges() {
        setup();
        Spi::run(
            "UPDATE graphwright.watch SET visibility = 'intersection' \
             WHERE source_table = 'notes'::regclass",
        )
        .unwrap();
        let a = edges_as("role_a");
        // sara-berlin is supported by row 2 (role_b) and row 4 (role_a);
        // role_a cannot see row 2, so intersection hides it.
        assert!(!a.contains(&("sara".into(), "berlin".into())));
        // single-row edges role_a fully sees still show.
        assert!(a.contains(&("sara".into(), "tehran".into())));
        assert!(a.contains(&("berlin".into(), "reza".into())));
    }

    // Lockdown: a direct catalog read is row-level-security filtered the same
    // way the accessor is, so the catalog is no privileged back door.
    #[pg_test]
    fn direct_catalog_read_is_rls_filtered() {
        setup();
        // Superuser bypasses RLS and sees the whole graph: 4 entities.
        let total = Spi::get_one::<i64>("SELECT count(*) FROM graphwright.entity")
            .unwrap()
            .unwrap();
        assert_eq!(total, 4);
        // role_b reads only row 2 (sara, berlin). The accessor shows that...
        assert_eq!(entities_as("role_b"), vec!["berlin", "sara"]);
        // ...and so does a direct SELECT on the catalog tables: no back door.
        // Each row contributes one edge_support row (4 total); role_b sees
        // only the one backed by its row.
        Spi::run("SET ROLE role_b").unwrap();
        let entity_direct = Spi::get_one::<i64>("SELECT count(*) FROM graphwright.entity")
            .unwrap()
            .unwrap();
        let support_direct = Spi::get_one::<i64>("SELECT count(*) FROM graphwright.edge_support")
            .unwrap()
            .unwrap();
        Spi::run("RESET ROLE").unwrap();
        assert_eq!(entity_direct, 2);
        assert_eq!(support_direct, 1);
    }

    // CREATE INDEX ... USING graphwright drives the same extraction with
    // ctid provenance, and the graph stays RLS-filtered per user.
    #[pg_test]
    fn create_index_builds_the_rls_filtered_graph() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("ALTER TABLE notes ENABLE ROW LEVEL SECURITY").unwrap();
        Spi::run("CREATE POLICY owner_can_read ON notes USING (owner = current_user)").unwrap();
        Spi::run("GRANT SELECT ON notes TO PUBLIC").unwrap();
        Spi::run("CREATE ROLE role_a").unwrap();
        Spi::run("CREATE ROLE role_b").unwrap();
        Spi::run(
            "INSERT INTO notes VALUES \
             (1, 'role_a', 'Sara Tehran'), \
             (2, 'role_b', 'Sara Berlin'), \
             (3, 'role_a', 'Reza Berlin'), \
             (4, 'role_a', 'Sara Berlin')",
        )
        .unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        let a = edges_as("role_a");
        assert!(a.contains(&("sara".into(), "berlin".into())));
        assert!(a.contains(&("sara".into(), "tehran".into())));
        assert!(a.contains(&("berlin".into(), "reza".into())));
        assert_eq!(edges_as("role_b"), vec![("sara".into(), "berlin".into())]);
    }

    // The whole graph, unfiltered (the test runs as superuser, which
    // bypasses RLS, so the probe sees every row).
    fn all_entities() -> Vec<String> {
        Spi::connect(|client| {
            let table = client.select(
                "SELECT surface FROM graphwright.entities('notes') ORDER BY surface",
                None,
                &[],
            )?;
            let mut v = Vec::new();
            for row in table {
                v.push(row.get::<String>(1)?.unwrap());
            }
            Ok::<_, pgrx::spi::Error>(v)
        })
        .unwrap()
    }

    // aminsert writes each change into index storage; maintain() re-resolves
    // the graph from there, and the RLS probe hides rows whose ctid is dead.
    #[pg_test]
    fn live_maintenance_applies_inserts_updates_deletes() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'Sara Tehran')").unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();
        assert_eq!(all_entities(), vec!["sara", "tehran"]);

        // INSERT marks the row; extraction is async, so it is not in the
        // graph until maintain() runs.
        Spi::run("INSERT INTO notes VALUES (2, 'amir', 'Reza Berlin')").unwrap();
        assert_eq!(all_entities(), vec!["sara", "tehran"]);
        Spi::run("SELECT graphwright.maintain()").unwrap();
        assert_eq!(all_entities(), vec!["berlin", "reza", "sara", "tehran"]);

        // UPDATE: the new row's tokens are stored under a new ctid; the old
        // ctid is now dead, so its tokens drop out at query time.
        Spi::run("UPDATE notes SET body = 'Sara Paris' WHERE id = 1").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();
        let updated = all_entities();
        assert!(updated.contains(&"paris".to_string()));
        assert!(!updated.contains(&"tehran".to_string())); // only row 1 had it

        // DELETE: the row's ctid goes dead, so its tokens drop out.
        Spi::run("DELETE FROM notes WHERE id = 2").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();
        let deleted = all_entities();
        assert!(!deleted.contains(&"reza".to_string()));
        assert!(!deleted.contains(&"berlin".to_string()));
        assert_eq!(deleted, vec!["paris", "sara"]);
    }

    // graphwright.maintain() drains every watch (the background worker's
    // per-tick body, exercised synchronously here).
    #[pg_test]
    fn maintain_drains_every_watch() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();
        // Insert after the (empty) build; the change is only queued.
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'Sara Tehran')").unwrap();
        assert!(all_entities().is_empty());

        let applied = Spi::get_one::<i64>("SELECT graphwright.maintain()")
            .unwrap()
            .unwrap();
        assert!(applied >= 1);
        assert_eq!(all_entities(), vec!["sara", "tehran"]);
    }

    // The per-row extraction is stored in the index relation's own pages
    // (WAL-logged), and reads back from there.
    #[pg_test]
    fn tokens_live_in_index_storage() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'Sara Tehran'), (2, 'amir', 'Reza Berlin')")
            .unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        let rows = Spi::connect(|client| {
            let table = client.select(
                "SELECT array_to_string(tokens, ',') FROM graphwright.index_dump('notes_kg') ORDER BY 1",
                None,
                &[],
            )?;
            let mut v = Vec::new();
            for row in table {
                v.push(row.get::<String>(1)?.unwrap());
            }
            Ok::<_, pgrx::spi::Error>(v)
        })
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|s| s.contains("sara") && s.contains("tehran")));
        assert!(rows
            .iter()
            .any(|s| s.contains("reza") && s.contains("berlin")));
    }

    // gc() (and ambulkdelete) reclaims storage records for deleted rows,
    // closing the ctid-reuse gap.
    #[pg_test]
    fn gc_reclaims_deleted_rows_from_storage() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'Sara Tehran'), (2, 'amir', 'Reza Berlin')")
            .unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();
        let count = || {
            Spi::get_one::<i64>("SELECT count(*) FROM graphwright.index_dump('notes_kg')")
                .unwrap()
                .unwrap()
        };
        assert_eq!(count(), 2);

        Spi::run("DELETE FROM notes WHERE id = 2").unwrap();
        let removed = Spi::get_one::<i64>("SELECT graphwright.gc('notes_kg')")
            .unwrap()
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count(), 1);

        let surviving = Spi::get_one::<String>(
            "SELECT array_to_string(tokens, ',') FROM graphwright.index_dump('notes_kg')",
        )
        .unwrap()
        .unwrap();
        assert!(surviving.contains("sara") && surviving.contains("tehran"));
    }

    // A configured extractor replaces the built-in tokenizer. Here a toy
    // SQL function keeps only capitalized words as entities, so the graph
    // holds the "entities", not every word.
    #[pg_test]
    fn custom_extractor_replaces_tokenization() {
        Spi::run(
            "CREATE FUNCTION public.caps(doc text) RETURNS text[] LANGUAGE sql AS $$ \
             SELECT array_agg(lower(w)) \
             FROM regexp_split_to_table(doc, '\\s+') AS w \
             WHERE w ~ '^[A-Z]' $$",
        )
        .unwrap();
        Spi::run("SET graphwright.extractor = 'public.caps'").unwrap();

        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'had coffee with Sara in Tehran')").unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        // Only Sara and Tehran survive, not had/coffee/with/in.
        assert_eq!(all_entities(), vec!["sara", "tehran"]);
    }

    // A relation extractor turns text into directed, typed edges instead of
    // undirected co-mention: "Joe closed Globex" becomes joe -closed-> globex.
    #[pg_test]
    fn relation_extractor_makes_typed_edges() {
        Spi::run(
            "CREATE FUNCTION public.caps(doc text) RETURNS text[] LANGUAGE sql AS $$ \
             SELECT array_agg(lower(w)) \
             FROM regexp_split_to_table(doc, '[^[:alpha:]]+') AS w \
             WHERE w ~ '^[[:upper:]]' $$",
        )
        .unwrap();
        Spi::run(
            "CREATE FUNCTION public.rels(doc text) RETURNS text[] LANGUAGE sql AS $$ \
             SELECT array_agg(part ORDER BY ord, idx) FROM ( \
               SELECT row_number() OVER () AS ord, m \
               FROM regexp_matches(doc, \
                 '([[:upper:]][[:alpha:]]+) (closed|signed) ([[:upper:]][[:alpha:]]+)', 'g') AS m \
             ) matches, \
             LATERAL unnest(ARRAY[lower(m[1]), m[2], lower(m[3])]) WITH ORDINALITY AS u(part, idx) $$",
        )
        .unwrap();
        Spi::run("SET graphwright.extractor = 'public.caps'").unwrap();
        Spi::run("SET graphwright.relation_extractor = 'public.rels'").unwrap();

        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'Joe closed Globex. Nadia signed Globex.')")
            .unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        let edges: Vec<(String, String, String)> = Spi::connect(|client| {
            let table = client.select(
                "SELECT src, predicate, dst FROM graphwright.edges('notes') ORDER BY src, dst",
                None,
                &[],
            )?;
            let mut out = Vec::new();
            for row in table {
                out.push((
                    row.get::<String>(1)?.expect("src"),
                    row.get::<String>(2)?.expect("predicate"),
                    row.get::<String>(3)?.expect("dst"),
                ));
            }
            Ok::<_, pgrx::spi::Error>(out)
        })
        .unwrap();

        // Directed and typed, and no co_mentioned edges: the relation lane
        // replaces co-mention when it is set.
        assert_eq!(
            edges,
            vec![
                (
                    "joe".to_string(),
                    "closed".to_string(),
                    "globex".to_string()
                ),
                (
                    "nadia".to_string(),
                    "signed".to_string(),
                    "globex".to_string()
                ),
            ]
        );
    }

    // The judge runs after the extractor and can drop mentions before they
    // reach the graph (here a larger model would decide; the toy judge just
    // removes a word).
    #[pg_test]
    fn judge_trims_extractor_output() {
        Spi::run(
            "CREATE FUNCTION public.words(doc text) RETURNS text[] LANGUAGE sql AS $$ \
             SELECT array_agg(lower(w)) FROM regexp_split_to_table(doc, '\\s+') AS w $$",
        )
        .unwrap();
        Spi::run(
            "CREATE FUNCTION public.drop_secret(doc text, ms text[]) RETURNS text[] \
             LANGUAGE sql AS $$ SELECT array_agg(m) FROM unnest(ms) AS m WHERE m <> 'secret' $$",
        )
        .unwrap();
        Spi::run("SET graphwright.extractor = 'public.words'").unwrap();
        Spi::run("SET graphwright.judge = 'public.drop_secret'").unwrap();

        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'sara secret tehran')").unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        // The extractor yields sara/secret/tehran; the judge drops 'secret'.
        assert_eq!(all_entities(), vec!["sara", "tehran"]);
    }

    // Exact resolution folds on the normalized key, so the Arabic and
    // Persian spellings of the same name become one entity.
    #[pg_test]
    fn normalization_folds_script_variants() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        // 'علي' uses Arabic yeh, 'علی' Persian yeh: same name, different codepoints.
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'علي'), (2, 'amir', 'علی')").unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();
        assert_eq!(all_entities().len(), 1);
    }

    // Phonetic keys propose a cross-script match that exact resolution
    // cannot reach: 'Faeze' (Latin) and 'فائزه' (Persian) share no
    // characters but the same consonant skeleton.
    #[pg_test]
    fn phonetic_proposals_bridge_scripts() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'Faeze'), (2, 'amir', 'فائزه')").unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        let pairs: Vec<(String, String)> = Spi::connect(|client| {
            let table = client.select(
                "SELECT surface_a, surface_b FROM graphwright.proposals('notes')",
                None,
                &[],
            )?;
            let mut v = Vec::new();
            for row in table {
                v.push((
                    row.get::<String>(1)?.unwrap(),
                    row.get::<String>(2)?.unwrap(),
                ));
            }
            Ok::<_, pgrx::spi::Error>(v)
        })
        .unwrap();

        assert_eq!(pairs.len(), 1);
        let (a, b) = &pairs[0];
        assert!(a == "faeze" || b == "faeze");
        assert!(a == "فائزه" || b == "فائزه");
    }

    // Distinctive phonetic matches auto-merge; short ones stay separate; a
    // human's split/merge overrides it, durably and reversibly.
    #[pg_test]
    fn gated_auto_merge_with_reversible_decisions() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run(
            "INSERT INTO notes VALUES \
             (1, 'amir', 'Khashayar'), (2, 'amir', 'خشایار'), \
             (3, 'amir', 'Ali'), (4, 'amir', 'علی')",
        )
        .unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        // Khashayar ~ خشایار auto-merge (distinctive); Ali / علی stay apart
        // (too short for the gate). So: 1 merged + 2 = 3 entities.
        assert_eq!(all_entities().len(), 3);

        // Split reverses the auto-merge (applies immediately).
        Spi::run("SELECT graphwright.split('notes', 'Khashayar', 'خشایار')").unwrap();
        assert_eq!(all_entities().len(), 4);

        // Merge the short pair the gate left alone.
        Spi::run("SELECT graphwright.merge('notes', 'Ali', 'علی')").unwrap();
        assert_eq!(all_entities().len(), 3);

        // Dropping the split lets the auto-merge return.
        Spi::run("SELECT graphwright.unmerge('notes', 'Khashayar', 'خشایار')").unwrap();
        assert_eq!(all_entities().len(), 2);
    }

    // A distinctive typo variant the phonetic skeleton forks on still
    // auto-merges through the fuzzy lane, and stays reversible.
    #[pg_test]
    fn gated_fuzzy_auto_merge_is_reversible() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run(
            "INSERT INTO notes VALUES \
             (1, 'sina', 'Shahrbanoodeylam'), (2, 'sina', 'Shahrbanoodeylan')",
        )
        .unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        // ~0.87 Jaccard, both past the gate: one entity.
        assert_eq!(all_entities().len(), 1);

        Spi::run("SELECT graphwright.split('notes', 'Shahrbanoodeylam', 'Shahrbanoodeylan')")
            .unwrap();
        assert_eq!(all_entities().len(), 2);
    }

    // Two people share a spelling, so the exact stage folds them into one
    // entity. A per-mention split separates the occurrence in one row, even
    // though the surfaces normalize identically; dropping it folds them back.
    #[pg_test]
    fn per_mention_split_separates_identical_spellings() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run("INSERT INTO notes VALUES (1, 'amir', 'Sara'), (2, 'amir', 'Sara')").unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        // Exact fold: two identical surfaces, one entity.
        assert_eq!(all_entities(), vec!["sara"]);

        // Split the second row's occurrence onto its own entity.
        let pk = Spi::get_one::<String>("SELECT (ctid)::text FROM notes WHERE id = 2")
            .unwrap()
            .unwrap();
        Spi::run(&format!(
            "SELECT graphwright.split_mention('notes', '{pk}', 'Sara')"
        ))
        .unwrap();
        assert_eq!(all_entities().len(), 2);

        // Reverse it: back to one.
        Spi::run(&format!(
            "SELECT graphwright.unsplit_mention('notes', '{pk}', 'Sara')"
        ))
        .unwrap();
        assert_eq!(all_entities(), vec!["sara"]);
    }

    // The embedding lane rescues short names the entropy gate keeps out of
    // the lexical lanes, and the merge is reversible like any other.
    #[pg_test]
    fn embedding_rescues_a_short_name_the_gate_drops() {
        Spi::run("CREATE TABLE notes (id int PRIMARY KEY, owner text, body text)").unwrap();
        Spi::run(
            "INSERT INTO notes VALUES (1, 'amir', 'Ali'), (2, 'amir', 'علی'), (3, 'amir', 'Reza')",
        )
        .unwrap();
        Spi::run("CREATE INDEX notes_kg ON notes USING graphwright (body)").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();

        // Ali / علی are too short for the lexical gate: three entities.
        assert_eq!(all_entities().len(), 3);

        // An embedder that maps both spellings of Ali to one vector merges
        // them, where fuzzy and phonetic were not allowed to look.
        Spi::run(
            "CREATE FUNCTION test_embed(t text) RETURNS float8[] LANGUAGE sql IMMUTABLE AS $$ \
             SELECT CASE WHEN t IN ('ali', 'علی') THEN ARRAY[1.0, 0.0] ELSE ARRAY[0.0, 1.0] END $$",
        )
        .unwrap();
        Spi::run("SET graphwright.embedder = 'test_embed'").unwrap();
        Spi::run("SELECT graphwright.maintain()").unwrap();
        assert_eq!(all_entities().len(), 2);

        // Reversible: a split vetoes the embedding merge.
        Spi::run("SELECT graphwright.split('notes', 'Ali', 'علی')").unwrap();
        assert_eq!(all_entities().len(), 3);
    }
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}

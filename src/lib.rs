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
// Milestone 1a proves that thesis with a deterministic stub extractor
// (tokenize a row, co-mention edges) and a manual `reindex`. The real
// index access method and an LLM/GLiNER extraction seam come later; they
// change how the graph is filled, not how it is filtered.

use pgrx::prelude::*;

pgrx::pg_module_magic!(name, version);

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

#[pg_schema]
mod graphwright {
    use super::{ident, lit, tokenize, watch_meta, Visibility};
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
    UNIQUE (watch_id, surface)
);

CREATE TABLE graphwright.mention (
    id           bigserial PRIMARY KEY,
    watch_id     integer NOT NULL REFERENCES graphwright.watch(id) ON DELETE CASCADE,
    entity_id    bigint NOT NULL REFERENCES graphwright.entity(id) ON DELETE CASCADE,
    source_pk    text NOT NULL,
    surface_form text NOT NULL
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
    source_pk text NOT NULL,
    PRIMARY KEY (edge_id, source_pk)
);

-- M1a exposes the graph only through the RLS-aware accessors, but those
-- run as the caller (SECURITY INVOKER), so the caller needs read access
-- to the catalog. Locking the catalog down (so the accessors are the
-- only door) is a later hardening step.
GRANT USAGE ON SCHEMA graphwright TO PUBLIC;
GRANT SELECT ON ALL TABLES IN SCHEMA graphwright TO PUBLIC;
"#,
        name = "catalog",
    );

    // Register a table's text column as a document source. Returns the
    // watch id. pk_column names the column used as provenance back to the
    // source row.
    #[pg_extern]
    fn watch(source_table: &str, text_column: &str, pk_column: &str) -> i32 {
        let sql = format!(
            "INSERT INTO graphwright.watch (source_table, text_column, pk_column) \
             VALUES ({}::regclass, {}, {}) \
             ON CONFLICT (source_table, text_column) \
             DO UPDATE SET pk_column = EXCLUDED.pk_column \
             RETURNING id",
            lit(source_table),
            lit(text_column),
            lit(pk_column),
        );
        Spi::get_one::<i32>(&sql)
            .expect("watch insert")
            .expect("watch id")
    }

    // Rebuild the whole graph for a watch from the current source rows.
    // The stub extractor: each token is an entity (exact-folded on its
    // normalized surface), consecutive tokens in a row are a co-mention
    // edge, and the source row is recorded as provenance. Runs with the
    // caller's privileges, so a full reindex must see every row (run it
    // as the table owner or a superuser).
    #[pg_extern]
    fn reindex(watch_id: i32) -> i64 {
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

        let mut mentions = 0i64;
        for (pk, body) in &rows {
            let mut ids: Vec<i64> = Vec::new();
            for tok in tokenize(body) {
                let entity_id = Spi::get_one::<i64>(&format!(
                    "INSERT INTO graphwright.entity (watch_id, surface) VALUES ({watch_id}, {surf}) \
                     ON CONFLICT (watch_id, surface) DO UPDATE SET surface = EXCLUDED.surface \
                     RETURNING id",
                    surf = lit(&tok),
                ))
                .expect("entity upsert")
                .expect("entity id");
                Spi::run(&format!(
                    "INSERT INTO graphwright.mention (watch_id, entity_id, source_pk, surface_form) \
                     VALUES ({watch_id}, {entity_id}, {pk}, {sf})",
                    pk = lit(pk),
                    sf = lit(&tok),
                ))
                .expect("mention insert");
                mentions += 1;
                ids.push(entity_id);
            }
            for pair in ids.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if a == b {
                    continue;
                }
                let (src, dst) = if a < b { (a, b) } else { (b, a) };
                let edge_id = Spi::get_one::<i64>(&format!(
                    "INSERT INTO graphwright.edge (watch_id, src, dst) VALUES ({watch_id}, {src}, {dst}) \
                     ON CONFLICT (watch_id, src, dst, predicate) DO UPDATE SET predicate = EXCLUDED.predicate \
                     RETURNING id",
                ))
                .expect("edge upsert")
                .expect("edge id");
                Spi::run(&format!(
                    "INSERT INTO graphwright.edge_support (edge_id, source_pk) VALUES ({edge_id}, {pk}) \
                     ON CONFLICT DO NOTHING",
                    pk = lit(pk),
                ))
                .expect("edge support");
            }
        }
        mentions
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

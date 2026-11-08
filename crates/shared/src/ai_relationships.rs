//! Join-graph inference for the AI profile: mined from saved SQL, guessed from column names, and
//! merged with declared foreign keys. Pure — no I/O.

use std::collections::HashSet;

use crate::models::{AiRelationship, RelationshipSource, SchemaTable};

/// `(from_table, from_column, to_table, to_column)` as written in an `ON a.x = b.y` clause, with
/// aliases resolved to table names (lowercased, quotes stripped).
pub type JoinPair = (String, String, String, String);

/// Words that can follow a table reference and must not be read as its alias.
const NOT_ALIAS: &[&str] = &[
    "on",
    "using",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "natural",
    "where",
    "group",
    "order",
    "limit",
    "having",
    "union",
    "intersect",
    "except",
    "offset",
    "window",
    "lateral",
    "set",
    "returning",
    "fetch",
];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Dot,
    Eq,
    Comma,
    Open,
    Other,
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '-' && c.get(i + 1) == Some(&'-') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if ch == '\'' {
            i += 1;
            while i < c.len() {
                if c[i] == '\'' {
                    if c.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Other);
        } else if ch == '"' || ch == '`' || ch == '[' {
            let close = if ch == '[' { ']' } else { ch };
            let start = i + 1;
            i = start;
            while i < c.len() && c[i] != close {
                i += 1;
            }
            out.push(Tok::Word(
                c[start..i.min(c.len())]
                    .iter()
                    .collect::<String>()
                    .to_lowercase(),
            ));
            i += 1;
        } else if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok::Word(
                c[start..i].iter().collect::<String>().to_lowercase(),
            ));
        } else {
            out.push(match ch {
                '.' => Tok::Dot,
                '=' => Tok::Eq,
                ',' => Tok::Comma,
                '(' => Tok::Open,
                _ => Tok::Other,
            });
            i += 1;
        }
    }
    out
}

/// Reads `a.b.c` starting at `i`; returns the parts and the next index.
fn dotted(t: &[Tok], mut i: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = vec![match t.get(i) {
        Some(Tok::Word(w)) => w.clone(),
        _ => return None,
    }];
    i += 1;
    while t.get(i) == Some(&Tok::Dot) {
        match t.get(i + 1) {
            Some(Tok::Word(w)) => parts.push(w.clone()),
            _ => break,
        }
        i += 2;
    }
    Some((parts, i))
}

/// Table reference with optional `[AS] alias`; records qualifier -> table in `aliases`.
fn table_ref(t: &[Tok], i: usize, aliases: &mut Vec<(String, String)>) -> usize {
    let Some((parts, mut i)) = dotted(t, i) else {
        return i;
    };
    let table = parts.join(".");
    aliases.push((table.clone(), table.clone()));
    if parts.len() > 1 {
        aliases.push((parts[parts.len() - 1].clone(), table.clone()));
    }
    if matches!(t.get(i), Some(Tok::Word(w)) if w == "as") {
        i += 1;
    }
    if let Some(Tok::Word(w)) = t.get(i) {
        if !NOT_ALIAS.contains(&w.as_str()) {
            aliases.push((w.clone(), table));
            i += 1;
        }
    }
    i
}

/// Extracts `JOIN ... ON x.c1 = y.c2` column pairs from one SQL string, resolving aliases to
/// tables.
///
/// Conditions whose sides aren't both `qualifier.column` (or whose qualifier is unknown, e.g. a
/// subquery alias) are skipped.
#[must_use]
#[allow(
    clippy::many_single_char_names,
    reason = "tokenizer cursor/side names (t, i, j, k, l, r) read clearer than long names"
)]
pub fn mine_join_pairs(sql: &str) -> Vec<JoinPair> {
    let t = tokenize(sql);
    let mut aliases: Vec<(String, String)> = Vec::new();
    // (qualifier, column, qualifier, column)
    let mut raw: Vec<(String, String, String, String)> = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let Tok::Word(w) = &t[i] else {
            i += 1;
            continue;
        };
        match w.as_str() {
            "from" | "join" => {
                i += 1;
                if t.get(i) == Some(&Tok::Open) {
                    continue;
                }
                i = table_ref(&t, i, &mut aliases);
                while w == "from" && t.get(i) == Some(&Tok::Comma) {
                    i = table_ref(&t, i + 1, &mut aliases);
                }
            }
            "on" => {
                i += 1;
                while let Some((l, j)) = dotted(&t, i) {
                    if l.len() < 2 || t.get(j) != Some(&Tok::Eq) {
                        break;
                    }
                    let Some((r, k)) = dotted(&t, j + 1) else {
                        break;
                    };
                    if r.len() < 2 {
                        break;
                    }
                    raw.push((
                        l[..l.len() - 1].join("."),
                        l[l.len() - 1].clone(),
                        r[..r.len() - 1].join("."),
                        r[r.len() - 1].clone(),
                    ));
                    i = k;
                    if matches!(t.get(i), Some(Tok::Word(a)) if a == "and") {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            _ => i += 1,
        }
    }
    // Later declarations win so an alias shadowing a table name resolves to what it aliases.
    let resolve = |q: &str| {
        aliases
            .iter()
            .rev()
            .find(|(a, _)| a == q)
            .map(|(_, t)| t.clone())
    };
    raw.into_iter()
        .filter_map(|(q1, c1, q2, c2)| Some((resolve(&q1)?, c1, resolve(&q2)?, c2)))
        .collect()
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Exact (case-insensitive) match, else a unique match on the unqualified name.
fn find_table<'a>(schema: &'a [SchemaTable], name: &str) -> Option<&'a SchemaTable> {
    if let Some(t) = schema.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
        return Some(t);
    }
    let mut hits = schema
        .iter()
        .filter(|t| last_segment(&t.name).eq_ignore_ascii_case(name));
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

fn find_column<'a>(t: &'a SchemaTable, col: &str) -> Option<&'a str> {
    t.columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(col))
        .map(|c| c.name.as_str())
}

/// `<singular>_id` -> `<plural>.id` guesses.
#[must_use]
pub fn name_heuristic(schema: &[SchemaTable]) -> Vec<AiRelationship> {
    let mut out = Vec::new();
    for from in schema {
        for col in &from.columns {
            let lower = col.name.to_lowercase();
            let Some(stem) = lower.strip_suffix("_id").filter(|s| !s.is_empty()) else {
                continue;
            };
            let mut candidates = vec![stem.to_string(), format!("{stem}s"), format!("{stem}es")];
            if let Some(base) = stem.strip_suffix('y') {
                candidates.push(format!("{base}ies"));
            }
            let target = schema
                .iter()
                .filter(|t| {
                    t.name != from.name
                        && candidates
                            .iter()
                            .any(|c| last_segment(&t.name).eq_ignore_ascii_case(c))
                })
                .find_map(|t| Some((t, find_column(t, "id")?)));
            if let Some((to, id)) = target {
                out.push(AiRelationship {
                    from_table: from.name.clone(),
                    from_column: col.name.clone(),
                    to_table: to.name.clone(),
                    to_column: id.to_string(),
                    source: RelationshipSource::Name,
                });
            }
        }
    }
    out
}

/// Canonicalizes names against `schema` (dropping anything that doesn't exist) and points the
/// edge at the `id` side when exactly one side is `id`.
fn canonical(schema: &[SchemaTable], r: &AiRelationship) -> Option<AiRelationship> {
    let ft = find_table(schema, &r.from_table)?;
    let fc = find_column(ft, &r.from_column)?;
    let tt = find_table(schema, &r.to_table)?;
    let tc = find_column(tt, &r.to_column)?;
    if ft.name == tt.name && fc == tc {
        return None;
    }
    let (a, b) = (
        (ft.name.clone(), fc.to_string()),
        (tt.name.clone(), tc.to_string()),
    );
    let swap = fc.eq_ignore_ascii_case("id") && !tc.eq_ignore_ascii_case("id");
    let ((from_table, from_column), (to_table, to_column)) = if swap { (b, a) } else { (a, b) };
    Some(AiRelationship {
        from_table,
        from_column,
        to_table,
        to_column,
        source: r.source,
    })
}

/// Union of all sources restricted to tables/columns that exist in `schema`. An edge seen from
/// several sources (in either direction) keeps the strongest: `fk > mined > name`.
#[must_use]
pub fn merge_relationships(
    fk: Vec<AiRelationship>,
    mined: Vec<JoinPair>,
    name: Vec<AiRelationship>,
    schema: &[SchemaTable],
) -> Vec<AiRelationship> {
    let mined =
        mined.into_iter().map(
            |(from_table, from_column, to_table, to_column)| AiRelationship {
                from_table,
                from_column,
                to_table,
                to_column,
                source: RelationshipSource::Mined,
            },
        );
    let mut all: Vec<AiRelationship> = fk
        .into_iter()
        .chain(mined)
        .chain(name)
        .filter_map(|r| canonical(schema, &r))
        .collect();
    all.sort_by_key(|r| r.source);
    let mut seen = HashSet::new();
    all.retain(|r| {
        let (a, b) = (
            (r.from_table.clone(), r.from_column.clone()),
            (r.to_table.clone(), r.to_column.clone()),
        );
        seen.insert(if a <= b { (a, b) } else { (b, a) })
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ColumnSchema;

    fn tbl(name: &str, cols: &[&str]) -> SchemaTable {
        SchemaTable {
            name: name.into(),
            columns: cols
                .iter()
                .map(|c| ColumnSchema {
                    name: (*c).into(),
                    type_name: None,
                })
                .collect(),
        }
    }

    fn p(a: &str, b: &str, c: &str, d: &str) -> JoinPair {
        (a.into(), b.into(), c.into(), d.into())
    }

    #[test]
    fn alias_with_and_without_as() {
        let sql = "SELECT * FROM orders o JOIN customers AS c ON o.customer_id = c.id";
        assert_eq!(
            mine_join_pairs(sql),
            vec![p("orders", "customer_id", "customers", "id")]
        );
    }

    #[test]
    fn no_alias_uses_table_name() {
        let sql =
            "select * from orders join customers on orders.customer_id = customers.id where 1=1";
        assert_eq!(
            mine_join_pairs(sql),
            vec![p("orders", "customer_id", "customers", "id")]
        );
    }

    #[test]
    fn schema_qualified_tables_and_quoting() {
        let sql =
            r#"SELECT 1 FROM "sales"."orders" o LEFT JOIN sales.items i ON i.order_id = o.id"#;
        assert_eq!(
            mine_join_pairs(sql),
            vec![p("sales.items", "order_id", "sales.orders", "id")]
        );
    }

    #[test]
    fn reversed_on_sides_are_oriented_at_merge() {
        let schema = [
            tbl("orders", &["id", "customer_id"]),
            tbl("customers", &["id"]),
        ];
        let sql = "SELECT * FROM customers c JOIN orders o ON c.id = o.customer_id";
        let rels = merge_relationships(vec![], mine_join_pairs(sql), vec![], &schema);
        assert_eq!(rels.len(), 1);
        assert_eq!(
            (
                rels[0].from_table.as_str(),
                rels[0].from_column.as_str(),
                rels[0].to_table.as_str()
            ),
            ("orders", "customer_id", "customers")
        );
    }

    #[test]
    fn self_join() {
        let sql = "SELECT * FROM employees e JOIN employees m ON e.manager_id = m.id";
        assert_eq!(
            mine_join_pairs(sql),
            vec![p("employees", "manager_id", "employees", "id")]
        );
    }

    #[test]
    fn multiple_conditions_and_joins() {
        let sql = "SELECT * FROM a JOIN b ON a.x = b.x AND a.y = b.y JOIN c ON c.b_id = b.id";
        assert_eq!(
            mine_join_pairs(sql),
            vec![
                p("a", "x", "b", "x"),
                p("a", "y", "b", "y"),
                p("c", "b_id", "b", "id")
            ]
        );
    }

    #[test]
    fn ignores_literals_comments_and_subquery_aliases() {
        let sql = "-- JOIN z ON z.a = z.b\nSELECT 'JOIN q ON q.a = q.b' FROM t JOIN (SELECT 1) s ON s.a = t.a";
        assert!(mine_join_pairs(sql).is_empty());
    }

    #[test]
    fn non_equi_and_expression_conditions_skipped() {
        let sql = "SELECT * FROM a JOIN b ON a.x > b.x JOIN c ON lower(c.k) = b.k";
        assert!(mine_join_pairs(sql).is_empty());
    }

    #[test]
    fn comma_from_list_aliases() {
        let sql = "SELECT * FROM orders o, customers c JOIN x ON x.oid = o.id";
        assert_eq!(mine_join_pairs(sql), vec![p("x", "oid", "orders", "id")]);
    }

    #[test]
    fn non_existent_column_or_table_dropped() {
        let schema = [
            tbl("orders", &["id", "customer_id"]),
            tbl("customers", &["id"]),
        ];
        let mined = vec![
            p("orders", "nope", "customers", "id"),
            p("orders", "customer_id", "ghosts", "id"),
            p("orders", "customer_id", "customers", "id"),
        ];
        let rels = merge_relationships(vec![], mined, vec![], &schema);
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].source, RelationshipSource::Mined);
    }

    #[test]
    fn mined_resolves_unqualified_name_to_schema_qualified_table() {
        let schema = [
            tbl("sales.orders", &["ID", "Customer_Id"]),
            tbl("customers", &["id"]),
        ];
        let mined = vec![p("orders", "customer_id", "customers", "id")];
        let rels = merge_relationships(vec![], mined, vec![], &schema);
        assert_eq!(rels[0].from_table, "sales.orders");
        assert_eq!(rels[0].from_column, "Customer_Id");
    }

    #[test]
    fn name_heuristic_plural_singular() {
        let schema = [
            tbl("customers", &["id", "name"]),
            tbl("categories", &["id"]),
            tbl(
                "orders",
                &["id", "customer_id", "category_id", "parent_id", "tenant_id"],
            ),
            tbl("address", &["id"]),
            tbl("shipments", &["id", "address_id", "order_id"]),
        ];
        let rels = name_heuristic(&schema);
        let got: Vec<_> = rels
            .iter()
            .map(|r| {
                (
                    r.from_table.as_str(),
                    r.from_column.as_str(),
                    r.to_table.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("orders", "customer_id", "customers"),
                ("orders", "category_id", "categories"),
                ("shipments", "address_id", "address"),
                ("shipments", "order_id", "orders"),
            ]
        );
        assert!(rels
            .iter()
            .all(|r| r.source == RelationshipSource::Name && r.to_column == "id"));
    }

    #[test]
    fn name_heuristic_needs_target_id_column() {
        let schema = [tbl("users", &["uid"]), tbl("posts", &["id", "user_id"])];
        assert!(name_heuristic(&schema).is_empty());
    }

    #[test]
    fn precedence_fk_over_mined_over_name_and_dedup() {
        let schema = [
            tbl("orders", &["id", "customer_id"]),
            tbl("customers", &["id"]),
        ];
        let rel = |src| AiRelationship {
            from_table: "orders".into(),
            from_column: "customer_id".into(),
            to_table: "customers".into(),
            to_column: "id".into(),
            source: src,
        };
        let mined = vec![p("orders", "customer_id", "customers", "id")];

        let all = merge_relationships(
            vec![rel(RelationshipSource::Fk)],
            mined.clone(),
            vec![rel(RelationshipSource::Name)],
            &schema,
        );
        assert_eq!(all, vec![rel(RelationshipSource::Fk)]);

        let no_fk =
            merge_relationships(vec![], mined, vec![rel(RelationshipSource::Name)], &schema);
        assert_eq!(no_fk, vec![rel(RelationshipSource::Mined)]);

        let only_name =
            merge_relationships(vec![], vec![], vec![rel(RelationshipSource::Name)], &schema);
        assert_eq!(only_name, vec![rel(RelationshipSource::Name)]);
    }

    #[test]
    fn reverse_direction_duplicate_collapses() {
        let schema = [
            tbl("orders", &["id", "customer_id"]),
            tbl("customers", &["id"]),
        ];
        let mined = vec![
            p("orders", "customer_id", "customers", "id"),
            p("customers", "id", "orders", "customer_id"),
        ];
        assert_eq!(merge_relationships(vec![], mined, vec![], &schema).len(), 1);
    }
}

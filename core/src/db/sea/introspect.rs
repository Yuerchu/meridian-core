//! The shape of an SQLite schema, read back from the database itself.
//!
//! Two things are built on this and must agree on what "the schema" is: the
//! generator that wrote the baseline migration from a database the 65 Diesel
//! migrations had produced, and the equivalence test that checks, on every run,
//! that the baseline still produces the same shape. Reading through the pragmas
//! rather than parsing `CREATE TABLE` text keeps both honest about the parts
//! SQLite itself tracks: columns, keys, foreign keys, indexes. What the pragmas
//! do not expose — `CHECK` clauses, a partial index's `WHERE`, trigger bodies —
//! is taken from `sqlite_master.sql`, with comments stripped and whitespace
//! normalised, since that text is the only place they exist.

use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};

/// Tables that belong to a migration tool rather than to the application. They
/// differ between a fresh database and a bridged one by design.
pub const LEDGER_TABLES: [&str; 2] = ["seaql_migrations", "__diesel_schema_migrations"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// In `sqlite_master` order, which is creation order.
    pub tables: Vec<Table>,
    /// Explicit `CREATE INDEX` statements, by name.
    pub indexes: Vec<Index>,
    pub triggers: Vec<Trigger>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// Column names in key order. One entry for a column-level key, several
    /// for a table-level `PRIMARY KEY (a, b)`.
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    /// Table-level and column-level `UNIQUE` constraints (the ones that make
    /// `sqlite_autoindex_*` entries), each as its column list.
    pub uniques: Vec<Vec<String>>,
    /// Every `CHECK (…)` clause, column-level and table-level alike, as the
    /// text between its parentheses with comments removed.
    pub checks: Vec<String>,
    /// `INTEGER PRIMARY KEY AUTOINCREMENT`: ids are never reused, which a
    /// test of migration 19 relies on.
    pub autoincrement: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// As declared: `TEXT`, `BIGINT`, `INTEGER`, `REAL`. The baseline declares
    /// the same affinities through sea-query's names, so comparisons go through
    /// [`affinity`].
    pub declared_type: String,
    pub not_null: bool,
    /// The default's SQL text as `pragma_table_info` reports it: `0`, `'user'`.
    pub default: Option<String>,
    pub in_primary_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub references_table: String,
    pub references_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub table: String,
    pub unique: bool,
    pub columns: Vec<IndexColumn>,
    /// The `WHERE` clause of a partial index, comments removed.
    pub where_clause: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexColumn {
    pub name: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub name: String,
    pub table: String,
    /// The whole `CREATE TRIGGER` statement, comments removed.
    pub sql: String,
}

fn raw(sql: String) -> Statement {
    Statement::from_string(DbBackend::Sqlite, sql)
}

fn quoted(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

impl Schema {
    /// Reads every application table, index and trigger on `conn`, leaving
    /// out the migration ledgers and SQLite's own tables.
    pub async fn read(conn: &impl ConnectionTrait) -> Result<Self, DbErr> {
        let objects = conn
            .query_all_raw(raw(format!(
                "SELECT type, name, tbl_name, sql FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' AND name NOT IN ({}) AND sql IS NOT NULL \
                 ORDER BY rowid",
                LEDGER_TABLES.map(|t| format!("'{t}'")).join(", ")
            )))
            .await?;

        let mut tables = Vec::new();
        let mut indexes = Vec::new();
        let mut triggers = Vec::new();
        for row in &objects {
            let kind: String = row.try_get("", "type")?;
            let name: String = row.try_get("", "name")?;
            let table: String = row.try_get("", "tbl_name")?;
            let sql: String = row.try_get("", "sql")?;
            let sql = strip_line_comments(&sql);
            match kind.as_str() {
                "table" => tables.push(read_table(conn, &name, &sql).await?),
                "index" => indexes.push(read_index(conn, &name, &table, &sql).await?),
                "trigger" => triggers.push(Trigger {
                    name,
                    table,
                    sql: normalize_whitespace(&sql),
                }),
                other => return Err(DbErr::Custom(format!("unexpected sqlite_master type {other:?}"))),
            }
        }
        indexes.sort_by(|a, b| a.name.cmp(&b.name));
        triggers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            tables,
            indexes,
            triggers,
        })
    }

    /// The same schema with everything that may legitimately differ between
    /// two builds of it removed: declared types reduced to their affinity,
    /// tables ordered by name, foreign keys and unique constraints sorted,
    /// checks and clauses whitespace-normalised. Two schemas whose normalised
    /// forms are equal behave the same under SQLite.
    pub fn normalized(&self) -> Self {
        let mut tables: Vec<Table> = self
            .tables
            .iter()
            .map(|table| {
                let mut foreign_keys = table.foreign_keys.clone();
                foreign_keys.sort();
                let mut uniques = table.uniques.clone();
                uniques.sort();
                let mut checks: Vec<String> = table
                    .checks
                    .iter()
                    .map(|c| strip_outer_parens(&normalize_whitespace(c)))
                    .collect();
                checks.sort();
                Table {
                    name: table.name.clone(),
                    columns: table
                        .columns
                        .iter()
                        .map(|column| Column {
                            name: column.name.clone(),
                            declared_type: affinity(&column.declared_type).to_owned(),
                            not_null: column.not_null,
                            default: column.default.clone(),
                            in_primary_key: column.in_primary_key,
                        })
                        .collect(),
                    primary_key: table.primary_key.clone(),
                    foreign_keys,
                    uniques,
                    checks,
                    autoincrement: table.autoincrement,
                }
            })
            .collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            tables,
            indexes: self
                .indexes
                .iter()
                .map(|index| Index {
                    where_clause: index.where_clause.as_deref().map(normalize_whitespace),
                    ..index.clone()
                })
                .collect(),
            triggers: self
                .triggers
                .iter()
                .map(|trigger| Trigger {
                    sql: normalize_whitespace(&trigger.sql),
                    ..trigger.clone()
                })
                .collect(),
        }
    }
}

async fn read_table(conn: &impl ConnectionTrait, name: &str, sql: &str) -> Result<Table, DbErr> {
    let mut columns = Vec::new();
    let mut key_positions: Vec<(i64, String)> = Vec::new();
    for row in conn
        .query_all_raw(raw(format!("PRAGMA table_info({})", quoted(name))))
        .await?
    {
        let column = Column {
            name: row.try_get("", "name")?,
            declared_type: row.try_get("", "type")?,
            not_null: row.try_get::<i64>("", "notnull")? != 0,
            default: row.try_get("", "dflt_value")?,
            in_primary_key: row.try_get::<i64>("", "pk")? != 0,
        };
        let pk: i64 = row.try_get("", "pk")?;
        if pk > 0 {
            key_positions.push((pk, column.name.clone()));
        }
        columns.push(column);
    }
    key_positions.sort();
    let primary_key = key_positions.into_iter().map(|(_, name)| name).collect();

    let mut foreign_keys: Vec<ForeignKey> = Vec::new();
    let mut last_id: Option<i64> = None;
    for row in conn
        .query_all_raw(raw(format!("PRAGMA foreign_key_list({})", quoted(name))))
        .await?
    {
        let id: i64 = row.try_get("", "id")?;
        let from: String = row.try_get("", "from")?;
        let to: String = row.try_get("", "to")?;
        if last_id == Some(id) {
            let fk = foreign_keys.last_mut().expect("a previous row with this id");
            fk.columns.push(from);
            fk.references_columns.push(to);
        } else {
            foreign_keys.push(ForeignKey {
                columns: vec![from],
                references_table: row.try_get("", "table")?,
                references_columns: vec![to],
                on_delete: row.try_get("", "on_delete")?,
                on_update: row.try_get("", "on_update")?,
            });
            last_id = Some(id);
        }
    }

    let mut uniques = Vec::new();
    for row in conn
        .query_all_raw(raw(format!("PRAGMA index_list({})", quoted(name))))
        .await?
    {
        let origin: String = row.try_get("", "origin")?;
        if origin == "u" {
            let index_name: String = row.try_get("", "name")?;
            uniques.push(
                index_columns(conn, &index_name)
                    .await?
                    .into_iter()
                    .map(|c| c.name)
                    .collect(),
            );
        }
    }

    let checks = check_clauses(sql);
    Ok(Table {
        name: name.to_owned(),
        columns,
        primary_key,
        foreign_keys,
        uniques,
        checks,
        autoincrement: sql.to_ascii_uppercase().contains("AUTOINCREMENT"),
    })
}

async fn read_index(conn: &impl ConnectionTrait, name: &str, table: &str, sql: &str) -> Result<Index, DbErr> {
    let unique = sql.trim_start().to_ascii_uppercase().starts_with("CREATE UNIQUE");
    Ok(Index {
        name: name.to_owned(),
        table: table.to_owned(),
        unique,
        columns: index_columns(conn, name).await?,
        where_clause: top_level_where(sql),
    })
}

async fn index_columns(conn: &impl ConnectionTrait, index: &str) -> Result<Vec<IndexColumn>, DbErr> {
    let mut columns = Vec::new();
    for row in conn
        .query_all_raw(raw(format!("PRAGMA index_xinfo({})", quoted(index))))
        .await?
    {
        if row.try_get::<i64>("", "key")? == 0 {
            continue;
        }
        let name: Option<String> = row.try_get("", "name")?;
        let name = name.ok_or_else(|| DbErr::Custom(format!("index {index} has an expression column")))?;
        columns.push(IndexColumn {
            name,
            descending: row.try_get::<i64>("", "desc")? != 0,
        });
    }
    Ok(columns)
}

/// SQLite's affinity rules (datatype3.html §3.1), in their order of precedence.
pub fn affinity(declared: &str) -> &'static str {
    let upper = declared.to_ascii_uppercase();
    if upper.contains("INT") {
        "INTEGER"
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        "TEXT"
    } else if upper.contains("BLOB") || upper.is_empty() {
        "BLOB"
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "REAL"
    } else {
        "NUMERIC"
    }
}

/// Removes `-- …` comments that are not inside a string literal. The newline
/// that ended the comment stays, so tokens on either side do not run together.
pub fn strip_line_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\'' {
                in_string = false;
            }
        } else if c == '\'' {
            in_string = true;
            out.push(c);
        } else if c == '-' && chars.peek() == Some(&'-') {
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `((a) OR (b))` → `(a) OR (b)`: one pair of parentheses that encloses the
/// whole expression carries no meaning, and sea-query adds its own around a
/// check.
fn strip_outer_parens(text: &str) -> String {
    let mut text = text.trim();
    while text.starts_with('(') && text.ends_with(')') && encloses_whole(text) {
        text = text[1..text.len() - 1].trim();
    }
    text.to_owned()
}

fn encloses_whole(text: &str) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    for (i, c) in text.char_indices() {
        if in_string {
            if c == '\'' {
                in_string = false;
            }
            continue;
        }
        match c {
            '\'' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != text.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

/// Every `CHECK (…)` clause in a `CREATE TABLE`, as the text inside its
/// parentheses. Comments must already be gone — a comment can say "check".
pub fn check_clauses(sql: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if c == '\'' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '\'' {
            in_string = true;
            i += 1;
            continue;
        }
        if sql.get(i..i + 5).is_some_and(|word| word.eq_ignore_ascii_case("CHECK"))
            && (i == 0 || !is_ident_char(bytes[i - 1] as char))
            && !sql[i + 5..].starts_with(is_ident_char)
        {
            let after = i + 5 + sql[i + 5..].len() - sql[i + 5..].trim_start().len();
            if bytes.get(after) == Some(&b'(') {
                let (inner, end) = balanced(sql, after);
                clauses.push(inner.trim().to_owned());
                i = end;
                continue;
            }
        }
        i += 1;
    }
    clauses
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Given `sql[open] == '('`, the text strictly inside the matching pair and
/// the index just past the closing parenthesis.
fn balanced(sql: &str, open: usize) -> (&str, usize) {
    let mut depth = 0usize;
    let mut in_string = false;
    for (offset, c) in sql[open..].char_indices() {
        if in_string {
            if c == '\'' {
                in_string = false;
            }
            continue;
        }
        match c {
            '\'' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let close = open + offset;
                    return (&sql[open + 1..close], close + 1);
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parentheses in {sql:?}");
}

/// The `WHERE` clause of a `CREATE INDEX`, which is the one at nesting depth
/// zero (a `WHERE` inside a subexpression would be inside parentheses).
pub fn top_level_where(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if c == '\'' {
                in_string = false;
            }
        } else {
            match c {
                '\'' => in_string = true,
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {
                    if depth == 0
                        && sql.get(i..i + 5).is_some_and(|word| word.eq_ignore_ascii_case("WHERE"))
                        && (i == 0 || !is_ident_char(bytes[i - 1] as char))
                        && !sql[i + 5..].starts_with(is_ident_char)
                    {
                        return Some(normalize_whitespace(&sql[i + 5..]));
                    }
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_go_but_strings_that_look_like_them_stay() {
        let sql = "a TEXT, -- a comment with CHECK in it\n b TEXT DEFAULT '--not a comment', c";
        assert_eq!(
            strip_line_comments(sql),
            "a TEXT, \n b TEXT DEFAULT '--not a comment', c"
        );
    }

    #[test]
    fn every_check_is_found_whether_on_a_column_or_the_table() {
        let sql = "CREATE TABLE t (\n  a TEXT NOT NULL CHECK (a IN ('x', 'y')),\n  b INTEGER CHECK(b > 0),\n  \
                   checked TEXT,\n  CHECK (\n    (a = 'x') OR (b IS NULL)\n  )\n)";
        assert_eq!(
            check_clauses(sql),
            vec!["a IN ('x', 'y')", "b > 0", "(a = 'x') OR (b IS NULL)"]
        );
    }

    #[test]
    fn a_partial_index_where_is_the_one_outside_parentheses() {
        assert_eq!(
            top_level_where("CREATE UNIQUE INDEX i\n  ON t(a)\n  WHERE state IN ('a',  'b')").as_deref(),
            Some("state IN ('a', 'b')")
        );
        assert_eq!(top_level_where("CREATE INDEX i ON t(a, b)"), None);
    }

    #[test]
    fn outer_parentheses_are_dropped_only_when_they_enclose_everything() {
        assert_eq!(strip_outer_parens("((a) OR (b))"), "(a) OR (b)");
        assert_eq!(strip_outer_parens("(a) OR (b)"), "(a) OR (b)");
        assert_eq!(strip_outer_parens("(x)"), "x");
    }

    #[test]
    fn affinity_follows_sqlite_precedence() {
        assert_eq!(affinity("BIGINT"), "INTEGER");
        assert_eq!(affinity("integer"), "INTEGER");
        assert_eq!(affinity("TEXT"), "TEXT");
        assert_eq!(affinity("VARCHAR(50)"), "TEXT");
        assert_eq!(affinity("REAL"), "REAL");
        assert_eq!(affinity("double"), "REAL");
        assert_eq!(affinity(""), "BLOB");
        assert_eq!(affinity("TIMESTAMP"), "NUMERIC");
    }
}

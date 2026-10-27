//! The tables of a machine, and how two machines agree on them before they exchange rows.
//!
//! Corrosion replicates rows, not tables. A change for a table or column that a machine does not
//! have yet is recorded as received and lost. So a link between two members carries Corrosion's
//! traffic only while both have the same tables and columns: each side sends its [`Tables`] when
//! the link starts and whenever they change, and applies what the other has and it lacks.
//!
//! Every schema statement, compiled in, applied at run time, or built from a member's tables,
//! passes [`check_statements`] first.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail, ensure};
use fallible_iterator::FallibleIterator;
use serde::{Deserialize, Serialize};
use sqlite3_parser::{
    ast::{Cmd, ColumnDefinition, CreateTableBody, Stmt},
    lexer::sql::Parser,
};

/// The most tables a member may send.
pub const MAX_TABLES: usize = 1000;

/// The replicated tables of a machine: imesh's own and the apps'.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tables(pub BTreeMap<String, Table>);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Table {
    /// The `CREATE TABLE` statement as SQLite keeps it, with added columns.
    pub sql: String,
    /// The `CREATE INDEX` statements of the table.
    pub indexes: Vec<String>,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// Lowercase: SQLite compares column names without case.
    pub name: String,
    pub ty: String,
    pub not_null: bool,
    pub default: Option<String>,
    /// Position in the primary key, 0 when not part of it.
    pub pk: i64,
}

impl Tables {
    /// The tables that a compiled schema makes, in a scratch database. The statements are
    /// rendered as Corrosion renders them (for example `key` as `KEY`), so both compare the same.
    pub fn from_sql(sql: &[String]) -> Result<Tables> {
        check_statements(sql)?;
        let c = rusqlite::Connection::open_in_memory()?;
        for s in sql {
            let mut p = Parser::new(s.as_bytes());
            while let Some(cmd) = p.next()? {
                c.execute_batch(&cmd.to_string())?;
            }
        }
        let tables = Tables::read(&c)?;
        // Such a column cannot be added to a table that has rows, so a machine with an older
        // version of the table could never take it, and its link would stay closed.
        for (name, t) in &tables.0 {
            if let Some(c) = t.columns.iter().find(|c| c.not_null && c.default.is_none() && c.pk == 0) {
                bail!("table {name}: column {} is NOT NULL without a default", c.name);
            }
        }
        Ok(tables)
    }

    /// Reads the replicated tables: everything but SQLite's, cr-sqlite's, and Corrosion's own.
    pub fn read(c: &rusqlite::Connection) -> rusqlite::Result<Tables> {
        let names: Vec<(String, String)> = c
            .prepare(
                "SELECT name, sql FROM sqlite_schema WHERE type = 'table' AND sql IS NOT NULL
                 AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND name NOT LIKE '\\_\\_%' ESCAPE '\\'
                 AND name NOT LIKE 'crsql\\_%' ESCAPE '\\' AND name NOT LIKE '%\\_\\_crsql\\_%' ESCAPE '\\'
                 ORDER BY name",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut tables = BTreeMap::new();
        for (name, sql) in names {
            let columns = c
                .prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1) ORDER BY cid")?
                .query_map([&name], |r| {
                    Ok(Column {
                        name: r.get::<_, String>(0)?.to_ascii_lowercase(),
                        ty: r.get::<_, String>(1)?.to_ascii_uppercase(),
                        not_null: r.get::<_, i64>(2)? != 0,
                        default: r.get(3)?,
                        pk: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let indexes = c
                .prepare(
                    "SELECT sql FROM sqlite_schema WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL ORDER BY name",
                )?
                .query_map([&name], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            tables.insert(name, Table { sql, indexes, columns });
        }
        Ok(Tables(tables))
    }

    /// Whether two machines can exchange rows safely: the same tables with the same columns.
    /// Indexes and statement text do not matter.
    pub fn same_shape(&self, other: &Tables) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().all(|(name, t)| other.0.get(name).is_some_and(|o| same_columns(&t.columns, &o.columns)))
    }

    /// The statements that bring this machine up to `other`: tables it lacks, and tables where
    /// `other` has more columns. An error names a table where both have columns the other lacks,
    /// or a column that differs; that needs a schema with both, applied on either machine.
    pub fn catch_up(&self, other: &Tables) -> Result<Vec<String>> {
        let mut sql = Vec::new();
        for (name, theirs) in &other.0 {
            match self.0.get(name) {
                None => {
                    sql.push(theirs.sql.clone());
                    sql.extend(theirs.indexes.iter().cloned());
                }
                Some(mine) if contains(&mine.columns, &theirs.columns) => {}
                Some(mine) if contains(&theirs.columns, &mine.columns) => {
                    // Our own definition plus their new columns, so Corrosion sees our columns as
                    // they are. A full table definition replaces the old one, indexes included,
                    // so keep ours too.
                    let add: Vec<&str> =
                        theirs.columns.iter().filter(|c| !mine.columns.contains(c)).map(|c| c.name.as_str()).collect();
                    sql.push(grow(&mine.sql, &theirs.sql, &add).with_context(|| format!("table {name}"))?);
                    let mut indexes = theirs.indexes.clone();
                    indexes.extend(mine.indexes.iter().filter(|i| !theirs.indexes.contains(i)).cloned());
                    sql.extend(indexes);
                }
                Some(mine) => {
                    let only = |a: &[Column], b: &[Column]| {
                        a.iter().filter(|c| !b.contains(c)).map(|c| c.name.clone()).collect::<Vec<_>>().join(", ")
                    };
                    bail!(
                        "table {name}: this machine has [{}], the member has [{}]",
                        only(&mine.columns, &theirs.columns),
                        only(&theirs.columns, &mine.columns)
                    );
                }
            }
        }
        Ok(sql)
    }

    /// For tables this machine has with every column of `other`'s: the statements that add
    /// `other`'s indexes that this machine lacks. Corrosion takes a full table definition, so each
    /// is this machine's table with its indexes and the new ones. Used at start, where the
    /// compiled schema may add only an index.
    pub fn missing_indexes(&self, other: &Tables) -> Vec<String> {
        let mut sql = Vec::new();
        for (name, theirs) in &other.0 {
            let Some(mine) = self.0.get(name) else { continue };
            let new: Vec<_> = theirs.indexes.iter().filter(|i| !mine.indexes.contains(i)).cloned().collect();
            if !new.is_empty() && contains(&mine.columns, &theirs.columns) {
                sql.push(mine.sql.clone());
                sql.extend(mine.indexes.iter().cloned());
                sql.extend(new);
            }
        }
        sql
    }
}

/// Accepts only `CREATE TABLE` and `CREATE INDEX` statements whose table, column, and index names
/// are plain (`[A-Za-z_][A-Za-z0-9_]*`) and not reserved. Reserved names start with `__`,
/// `sqlite_`, or `crsql`, or contain `__crsql_`: the tables of SQLite, cr-sqlite, and Corrosion,
/// which [`Tables::read`] leaves out, so the gate could not protect their rows.
pub fn check_statements(sql: &[String]) -> Result<()> {
    for s in sql {
        let mut p = Parser::new(s.as_bytes());
        while let Some(cmd) = p.next()? {
            match &cmd {
                Cmd::Stmt(Stmt::CreateTable { tbl_name, body, .. }) => {
                    check_name("table", &tbl_name.name.0)?;
                    let CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
                        bail!("CREATE TABLE ... AS SELECT is not allowed in a schema");
                    };
                    for c in columns {
                        check_name("column", &c.col_name.0)?;
                    }
                }
                Cmd::Stmt(Stmt::CreateIndex { idx_name, tbl_name, .. }) => {
                    check_name("index", &idx_name.name.0)?;
                    check_name("table", &tbl_name.0)?;
                }
                other => bail!("a schema has only CREATE TABLE and CREATE INDEX statements, not: {other}"),
            }
        }
    }
    Ok(())
}

/// The tables that `CREATE TABLE` and `CREATE INDEX` statements name.
pub fn table_names(sql: &[String]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for s in sql {
        let mut p = Parser::new(s.as_bytes());
        while let Some(cmd) = p.next()? {
            match cmd {
                Cmd::Stmt(Stmt::CreateTable { tbl_name, .. }) => names.push(unquote(&tbl_name.name.0).to_string()),
                Cmd::Stmt(Stmt::CreateIndex { tbl_name, .. }) => names.push(unquote(&tbl_name.0).to_string()),
                _ => {}
            }
        }
    }
    Ok(names)
}

fn check_name(what: &str, raw: &str) -> Result<()> {
    let name = unquote(raw);
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ensure!(plain, "{what} name {raw} may have only letters, digits, and _");
    let n = name.to_ascii_lowercase();
    let reserved = n.starts_with("__") || n.starts_with("sqlite_") || n.starts_with("crsql") || n.contains("__crsql_");
    ensure!(
        !reserved,
        "{what} name {name} is reserved: names may not start with __, sqlite_, or crsql, or contain __crsql_"
    );
    Ok(())
}

fn unquote(name: &str) -> &str {
    name.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']' | '\''))
}

/// The `CREATE TABLE` statement `mine` with the columns named `add` taken from `theirs`.
fn grow(mine: &str, theirs: &str, add: &[&str]) -> Result<String> {
    fn parse(sql: &str) -> Result<Cmd> {
        match Parser::new(sql.as_bytes()).next()? {
            Some(cmd @ Cmd::Stmt(Stmt::CreateTable { .. })) => Ok(cmd),
            _ => Err(anyhow!("not a CREATE TABLE statement: {sql}")),
        }
    }
    fn columns(cmd: &mut Cmd) -> Result<&mut Vec<ColumnDefinition>> {
        match cmd {
            Cmd::Stmt(Stmt::CreateTable { body: CreateTableBody::ColumnsAndConstraints { columns, .. }, .. }) => {
                Ok(columns)
            }
            _ => Err(anyhow!("a table without a column list")),
        }
    }
    let mut out = parse(mine)?;
    let mut other = parse(theirs)?;
    let new: Vec<_> = columns(&mut other)?
        .iter()
        .filter(|c| add.iter().any(|a| unquote(&c.col_name.0).eq_ignore_ascii_case(a)))
        .cloned()
        .collect();
    ensure!(new.len() == add.len(), "cannot find columns {add:?} in the member's table");
    columns(&mut out)?.extend(new);
    Ok(out.to_string())
}

fn same_columns(a: &[Column], b: &[Column]) -> bool {
    a.len() == b.len() && contains(a, b)
}

/// Whether `big` has every column of `small`, with the same definition.
fn contains(big: &[Column], small: &[Column]) -> bool {
    small.iter().all(|c| big.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(sql: &str) -> Tables {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(sql).unwrap();
        Tables::read(&c).unwrap()
    }

    #[test]
    fn a_new_table_and_new_columns_catch_up() {
        let old = tables("CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, a TEXT);");
        let new = tables(
            "CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, a TEXT, b INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX t_b ON t (b);
             CREATE TABLE u (k TEXT NOT NULL PRIMARY KEY);",
        );
        assert!(!old.same_shape(&new));
        let sql = old.catch_up(&new).unwrap();
        assert_eq!(sql.len(), 3, "{sql:?}");
        // The newer machine needs nothing from the older one.
        assert!(new.catch_up(&old).unwrap().is_empty());
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, a TEXT);").unwrap();
        c.execute_batch("ALTER TABLE t ADD COLUMN b INTEGER NOT NULL DEFAULT 0; CREATE INDEX t_b ON t (b); CREATE TABLE u (k TEXT NOT NULL PRIMARY KEY);").unwrap();
        assert!(Tables::read(&c).unwrap().same_shape(&new), "ALTER and CREATE give the same shape");
    }

    #[test]
    fn columns_added_on_both_sides_conflict() {
        let a = tables("CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, x TEXT);");
        let b = tables("CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, y TEXT);");
        let err = a.catch_up(&b).unwrap_err().to_string();
        assert!(err.contains("[x]") && err.contains("[y]"), "{err}");
    }

    #[test]
    fn a_changed_column_type_conflicts() {
        let a = tables("CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, x TEXT);");
        let b = tables("CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, x INTEGER);");
        assert!(a.catch_up(&b).is_err());
        assert!(!a.same_shape(&b));
    }

    #[test]
    fn growing_keeps_this_machines_column_text() {
        let mine = tables("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, a TEXT DEFAULT (0));");
        let theirs = tables("CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, a TEXT DEFAULT (0), b TEXT);");
        let sql = mine.catch_up(&theirs).unwrap();
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains("PRIMARY KEY NOT NULL"), "{}", sql[0]);
        assert!(sql[0].to_lowercase().contains("b text"), "{}", sql[0]);
        assert!(Tables::from_sql(&sql).unwrap().same_shape(&theirs));
    }

    #[test]
    fn reserved_names_are_found_by_parsing() {
        assert!(check_statements(&["CREATE TABLE \"__x\" (a); CREATE INDEX i ON app (a);".to_string()]).is_err());
        assert!(check_statements(&["CREATE INDEX i ON app (a)".to_string()]).is_ok());
        assert!(check_statements(&["CREATE TABLE \"a'b\" (a)".to_string()]).is_err(), "quote in a name");
        assert!(check_statements(&["CREATE TABLE t (\"we ird\" TEXT)".to_string()]).is_err(), "space in a column");
        assert!(check_statements(&["DROP INDEX IF EXISTS i".to_string()]).is_err());
    }

    #[test]
    fn reserved_table_names_are_refused() {
        for sql in [
            "CREATE TABLE __x (a)",
            "CREATE TABLE sqlite_x (a)",
            "CREATE TABLE crsql_x (a)",
            "CREATE TABLE a__crsql_b (a)",
        ] {
            assert!(Tables::from_sql(&[sql.to_string()]).is_err(), "{sql}");
        }
        assert!(Tables::from_sql(&["CREATE TABLE app (id TEXT NOT NULL PRIMARY KEY)".to_string()]).is_ok());
    }

    #[test]
    fn statements_are_compared_as_corrosion_renders_them() {
        let written = Tables::from_sql(&[
            "CREATE TABLE t (key TEXT NOT NULL PRIMARY KEY, value TEXT NOT NULL DEFAULT '')".to_string(),
        ])
        .unwrap();
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (KEY TEXT NOT NULL PRIMARY KEY, VALUE TEXT NOT NULL DEFAULT '')").unwrap();
        let stored = Tables::read(&c).unwrap();
        assert!(written.same_shape(&stored));
        assert!(stored.catch_up(&written).unwrap().is_empty());
    }

    #[test]
    fn a_compiled_schema_is_tables_and_indexes_only() {
        assert!(
            Tables::from_sql(&["CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY); CREATE VIEW v AS SELECT 1".to_string()])
                .is_err()
        );
        assert!(
            Tables::from_sql(&["CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, n INTEGER NOT NULL)".to_string()])
                .is_err()
        );
    }

    #[test]
    fn an_index_alone_is_found() {
        let old = Tables::from_sql(&["CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, a TEXT)".to_string()]).unwrap();
        let new = Tables::from_sql(&[
            "CREATE TABLE t (id TEXT NOT NULL PRIMARY KEY, a TEXT); CREATE INDEX t_a ON t (a)".to_string(),
        ])
        .unwrap();
        assert!(old.catch_up(&new).unwrap().is_empty());
        assert_eq!(old.missing_indexes(&new).len(), 2);
        assert!(new.missing_indexes(&new).is_empty());
    }

    #[test]
    fn internal_tables_are_left_out() {
        let t = tables(
            "CREATE TABLE app (id TEXT NOT NULL PRIMARY KEY); CREATE TABLE __corro_x (a); CREATE TABLE app__crsql_clock (a);",
        );
        assert_eq!(t.0.keys().collect::<Vec<_>>(), ["app"]);
    }
}

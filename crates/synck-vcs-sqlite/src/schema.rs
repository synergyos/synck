//! Structural SQLite compatibility, independent of CREATE statement formatting.
use rusqlite::Connection;
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
pub struct SchemaShape {
    tables: Vec<Table>,
    other: Vec<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
struct Table {
    name: String,
    strict: bool,
    columns: Vec<Column>,
    unique: Vec<UniqueKey>,
    foreign: Vec<Vec<ForeignKeyPart>>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct UniqueKey {
    columns: Vec<IndexColumn>,
    expression: Option<Vec<String>>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Column {
    name: String,
    data_type: String,
    not_null: bool,
    default: Option<Vec<String>>,
    primary_key_position: i64,
    hidden: i64,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IndexColumn {
    name: Option<String>,
    descending: bool,
    collation: String,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ForeignKeyPart {
    table: String,
    from: String,
    to: Option<String>,
    on_update: String,
    on_delete: String,
    match_rule: String,
}

/// Compare required relations and uniqueness, not physical column order or
/// performance indexes. CHECK predicates are enforced by record validation;
/// SQLite does not expose them structurally. Triggers/views remain observable
/// so an adapter cannot silently add executable behavior to owned relations.
pub fn inspect(connection: &Connection, prefix: &str) -> rusqlite::Result<SchemaShape> {
    let pattern = format!("{prefix}*");
    let objects = connection.prepare(
        "SELECT type,name FROM sqlite_schema WHERE sql IS NOT NULL AND (name GLOB ?1 OR tbl_name GLOB ?1) ORDER BY type,name"
    )?.query_map([&pattern], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tables = Vec::new();
    let mut other = Vec::new();
    for (kind, name) in objects {
        if kind == "index" {
            continue;
        }
        if kind != "table" {
            other.push((kind, name));
            continue;
        }
        let strict = connection.query_row(
            "SELECT strict FROM pragma_table_list WHERE schema='main' AND name=?1",
            [&name],
            |row| row.get(0),
        )?;
        let mut columns = connection.prepare("SELECT name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo(?1) ORDER BY name")?.query_map([&name], |row| {
            let ty: String = row.get(1)?;
            let ty = match ty.to_ascii_uppercase().as_str() { "INT" => "INTEGER".into(), other => other.to_string() };
            let default: Option<String> = row.get(3)?;
            Ok(Column {
                name: row.get(0)?, data_type: ty, not_null: row.get(2)?,
                default: default.map(|value| tokens(&value)).transpose()?,
                primary_key_position: row.get(4)?, hidden: row.get(5)?,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        columns.sort();
        let indexes = connection
            .prepare("SELECT name,partial FROM pragma_index_list(?1) WHERE \"unique\"=1")?
            .query_map([&name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut unique = Vec::new();
        for (index, partial) in indexes {
            let keys = connection.prepare("SELECT name,\"desc\",coll FROM pragma_index_xinfo(?1) WHERE key=1 ORDER BY seqno")?.query_map([&index], |row| Ok(IndexColumn {
                name: row.get(0)?, descending: row.get(1)?,
                collation: row.get::<_,String>(2)?.to_ascii_uppercase(),
            }))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let expression = if partial || keys.iter().any(|key| key.name.is_none()) {
                let sql: String = connection.query_row(
                    "SELECT sql FROM sqlite_schema WHERE name=?1",
                    [&index],
                    |row| row.get(0),
                )?;
                let parsed = tokens(&sql)?;
                let start = parsed
                    .iter()
                    .position(|token| token == "(")
                    .ok_or(rusqlite::Error::InvalidQuery)?;
                Some(parsed[start..].to_vec())
            } else {
                None
            };
            unique.push(UniqueKey {
                columns: keys,
                expression,
            });
        }
        unique.sort();
        unique.dedup();
        let rows = connection.prepare("SELECT id,\"table\",\"from\",\"to\",on_update,on_delete,\"match\" FROM pragma_foreign_key_list(?1) ORDER BY id,seq")?.query_map([&name], |row| Ok((row.get::<_,i64>(0)?, ForeignKeyPart {
            table: row.get(1)?, from: row.get(2)?, to: row.get(3)?,
            on_update: row.get(4)?, on_delete: row.get(5)?, match_rule: row.get(6)?,
        })))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut groups = BTreeMap::<i64, Vec<_>>::new();
        for (id, row) in rows {
            groups.entry(id).or_default().push(row);
        }
        let mut foreign: Vec<_> = groups.into_values().collect();
        foreign.sort();
        tables.push(Table {
            name,
            strict,
            columns,
            unique,
            foreign,
        });
    }
    Ok(SchemaShape { tables, other })
}

// Only uniqueness expressions and defaults need SQL tokens: PRAGMA exposes all
// other structural fields. Keep string literals distinct from quoted identifiers.
fn tokens(sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut input = sql.chars().peekable();
    let mut output = Vec::new();
    while let Some(c) = input.next() {
        if c.is_whitespace() || c == ';' {
            continue;
        }
        if c == '-' && input.peek() == Some(&'-') {
            input.next();
            for c in input.by_ref() {
                if c == '\n' {
                    break;
                }
            }
            continue;
        }
        if c == '/' && input.peek() == Some(&'*') {
            input.next();
            loop {
                match input.next() {
                    Some('*') if input.peek() == Some(&'/') => {
                        input.next();
                        break;
                    }
                    Some(_) => {}
                    None => return Err(rusqlite::Error::InvalidQuery),
                }
            }
            continue;
        }
        if matches!(c, '\'' | '"' | '`' | '[') {
            let close = if c == '[' { ']' } else { c };
            let mut value = String::new();
            loop {
                let next = input.next().ok_or(rusqlite::Error::InvalidQuery)?;
                if next == close {
                    if c != '[' && input.peek() == Some(&close) {
                        input.next();
                        value.push(close);
                    } else {
                        break;
                    }
                } else {
                    value.push(next);
                }
            }
            output.push(if c == '\'' {
                format!("string:{value}")
            } else {
                format!("word:{}", value.to_ascii_lowercase())
            });
        } else if c.is_alphanumeric() || c == '_' {
            let mut value = c.to_string();
            while input
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                value.push(input.next().unwrap());
            }
            output.push(format!("word:{}", value.to_ascii_lowercase()));
        } else {
            output.push(c.to_string());
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn equivalent_layouts_and_index_names_have_the_same_shape() {
        let a = Connection::open_in_memory().unwrap();
        let b = Connection::open_in_memory().unwrap();
        a.execute_batch("CREATE TABLE vcs_example(id TEXT PRIMARY KEY, parent TEXT, name TEXT NOT NULL, live INTEGER NOT NULL DEFAULT 1) STRICT; CREATE UNIQUE INDEX first ON vcs_example(COALESCE(parent,''),name) WHERE live=1;").unwrap();
        b.execute_batch("create table vcs_example (live INT not null default 1, name TEXT not null, parent TEXT, id TEXT primary key) strict; create unique index other on vcs_example ( coalesce ( \"parent\" , '' ) , [name] ) where live = 1; CREATE INDEX extra ON vcs_example(name);").unwrap();
        assert_eq!(inspect(&a, "vcs_").unwrap(), inspect(&b, "vcs_").unwrap());
        b.execute_batch("DROP INDEX other; CREATE UNIQUE INDEX other ON vcs_example(COALESCE(parent,'x'), name) WHERE live=1;").unwrap();
        assert_ne!(inspect(&a, "vcs_").unwrap(), inspect(&b, "vcs_").unwrap());
    }
}

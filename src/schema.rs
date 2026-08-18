//! DB-schema queries over the JSON that `php artisan schema:svg --format=json`
//! emits (`{tables:[{name,columns:[{name,type}]}], foreignKeys:[...]}`). This is
//! the live schema Laravel introspects, so plouf.rs answers table-structure
//! questions without re-parsing migrations.

use std::fs;
use std::io;

use serde_json::Value;

struct Column {
    name: String,
    ty: String,
}

struct Table {
    name: String,
    columns: Vec<Column>,
}

struct ForeignKey {
    from_table: String,
    from_columns: Vec<String>,
    to_table: String,
    to_columns: Vec<String>,
}

struct Schema {
    tables: Vec<Table>,
    fks: Vec<ForeignKey>,
}

fn str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(ToString::to_string)).collect())
        .unwrap_or_default()
}

fn table_rec(v: &Value) -> Option<Table> {
    let name = v["name"].as_str()?.to_string();
    let columns = v["columns"]
        .as_array()?
        .iter()
        .filter_map(|c| Some(Column { name: c["name"].as_str()?.to_string(), ty: c["type"].as_str()?.to_string() }))
        .collect();
    Some(Table { name, columns })
}

fn fk_rec(v: &Value) -> Option<ForeignKey> {
    Some(ForeignKey {
        from_table: v["from_table"].as_str()?.to_string(),
        from_columns: str_array(&v["from_columns"]),
        to_table: v["to_table"].as_str()?.to_string(),
        to_columns: str_array(&v["to_columns"]),
    })
}

fn parse_schema(text: &str) -> Result<Schema, io::Error> {
    let v: Value = serde_json::from_str(text).map_err(io::Error::other)?;
    let tables = v["tables"].as_array().map(|a| a.iter().filter_map(table_rec).collect()).unwrap_or_default();
    let fks = v["foreignKeys"].as_array().map(|a| a.iter().filter_map(fk_rec).collect()).unwrap_or_default();
    Ok(Schema { tables, fks })
}

fn load(path: &str) -> Result<Schema, io::Error> {
    let text = fs::read_to_string(path)?;
    parse_schema(&text)
}

/// List every table name (alphabetical).
pub fn list_tables(path: &str) -> Result<(), io::Error> {
    let schema = load(path)?;
    let mut names: Vec<&str> = schema.tables.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    for n in names {
        println!("{n}");
    }
    Ok(())
}

/// Print one table's columns, its outgoing FKs, and who references it.
pub fn table(path: &str, name: &str) -> Result<(), io::Error> {
    let schema = load(path)?;
    let Some(t) = schema.tables.iter().find(|t| t.name == name) else {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no table '{name}'")));
    };

    println!("table {}", t.name);
    for c in &t.columns {
        println!("  {}: {}", c.name, c.ty);
    }

    let outgoing: Vec<&ForeignKey> = schema.fks.iter().filter(|f| f.from_table == name).collect();
    if !outgoing.is_empty() {
        println!("references:");
        for f in outgoing {
            println!("  {} -> {}.{}", f.from_columns.join(","), f.to_table, f.to_columns.join(","));
        }
    }

    let incoming: Vec<&ForeignKey> = schema.fks.iter().filter(|f| f.to_table == name).collect();
    if !incoming.is_empty() {
        println!("referenced by:");
        for f in incoming {
            println!("  {}.{} -> {}", f.from_table, f.from_columns.join(","), f.to_columns.join(","));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::parse_schema;

    const FIXTURE: &str = r#"{
        "tables": [
            {"name": "companies", "columns": [{"name":"id","type":"bigint"},{"name":"name","type":"varchar"}]},
            {"name": "animals", "columns": [{"name":"id","type":"bigint"},{"name":"company_id","type":"bigint"}]}
        ],
        "foreignKeys": [
            {"from_table":"animals","from_columns":["company_id"],"to_table":"companies","to_columns":["id"]}
        ]
    }"#;

    #[test]
    fn parses_tables_columns_and_fks() {
        let s = parse_schema(FIXTURE).unwrap();
        assert_eq!(s.tables.len(), 2);
        let companies = s.tables.iter().find(|t| t.name == "companies").unwrap();
        assert_eq!(companies.columns.len(), 2);
        assert_eq!(companies.columns[0].name, "id");
        assert_eq!(companies.columns[0].ty, "bigint");
        assert_eq!(s.fks.len(), 1);
        assert_eq!(s.fks[0].from_table, "animals");
        assert_eq!(s.fks[0].to_table, "companies");
        assert_eq!(s.fks[0].from_columns[0], "company_id");
    }

    #[test]
    fn tolerates_missing_sections() {
        let s = parse_schema("{}").unwrap();
        assert!(s.tables.is_empty());
        assert!(s.fks.is_empty());
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(parse_schema("not json").is_err());
    }
}

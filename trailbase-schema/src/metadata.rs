use jsonschema::Validator;
use lazy_static::lazy_static;
use log::*;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

use crate::sqlite::{Column, ColumnDataType, ColumnOption, Table, View};

// TODO: Can we merge this with crate::sqlite::SchemaError?
#[derive(Debug, Clone, Error)]
pub enum JsonSchemaError {
  #[error("Schema compile error: {0}")]
  SchemaCompile(String),
  #[error("Validation error")]
  Validation,
  #[error("Schema not found: {0}")]
  NotFound(String),
  #[error("Json serialization error: {0}")]
  JsonSerialization(Arc<serde_json::Error>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum JsonColumnMetadata {
  SchemaName(String),
  Pattern(serde_json::Value),
}

impl JsonColumnMetadata {
  pub fn validate(&self, value: &serde_json::Value) -> Result<(), JsonSchemaError> {
    match self {
      Self::SchemaName(name) => {
        let Some(schema) = crate::registry::get_compiled_schema(name) else {
          return Err(JsonSchemaError::NotFound(name.to_string()));
        };
        schema
          .validate(value)
          .map_err(|_err| JsonSchemaError::Validation)?;
        return Ok(());
      }
      Self::Pattern(pattern) => {
        let schema =
          Validator::new(pattern).map_err(|err| JsonSchemaError::SchemaCompile(err.to_string()))?;
        if !schema.is_valid(value) {
          Err(JsonSchemaError::Validation)
        } else {
          Ok(())
        }
      }
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JsonMetadata {
  pub columns: Vec<Option<JsonColumnMetadata>>,

  // Contains both, 'std.FileUpload' and 'std.FileUpload'.
  file_column_indexes: Vec<usize>,
}

impl JsonMetadata {
  pub fn has_file_columns(&self) -> bool {
    return !self.file_column_indexes.is_empty();
  }

  /// Contains both, 'std.FileUpload' and 'std.FileUpload'.
  pub fn file_column_indexes(&self) -> &[usize] {
    return &self.file_column_indexes;
  }

  fn from_table(table: &Table) -> Self {
    return Self::from_columns(&table.columns);
  }

  fn from_view(view: &View) -> Option<Self> {
    return view.columns.as_ref().map(|cols| Self::from_columns(cols));
  }

  fn from_columns(columns: &[Column]) -> Self {
    let columns: Vec<_> = columns.iter().map(build_json_metadata).collect();

    let file_column_indexes = find_file_column_indexes(&columns);

    return Self {
      columns,
      file_column_indexes,
    };
  }
}

/// A data class describing a sqlite Table and additional meta data useful for TrailBase.
///
/// An example of TrailBase idiosyncrasies are UUIDv7 columns, which are a bespoke concept.
#[derive(Debug, Clone)]
pub struct TableMetadata {
  pub schema: Table,

  /// If and which column on this table qualifies as a record PK column, i.e. integer or UUIDv7.
  pub record_pk_column: Option<usize>,
  /// If and which columns on this table reference _user(id).
  pub user_id_columns: Vec<usize>,
  /// Metadata for CHECK(json_schema()) columns.
  pub json_metadata: JsonMetadata,

  name_to_index: HashMap<String, usize>,
  // TODO: Add triggers once sqlparser supports a sqlite "CREATE TRIGGER" statements.
}

impl TableMetadata {
  /// Build a new TableMetadata instance containing TrailBase/RecordApi specific information.
  ///
  /// NOTE: The list of all tables is needed only to extract interger/UUIDv7 pk columns for foreign
  /// key relationships.
  pub fn new(table: Table, tables: &[Table], user_table_name: &str) -> Self {
    let name_to_index = HashMap::<String, usize>::from_iter(
      table
        .columns
        .iter()
        .enumerate()
        .map(|(index, col)| (col.name.clone(), index)),
    );

    let record_pk_column = find_record_pk_column_index(&table.columns, tables);
    let user_id_columns = find_user_id_foreign_key_columns(&table.columns, user_table_name);
    let json_metadata = JsonMetadata::from_table(&table);

    return TableMetadata {
      schema: table,
      name_to_index,
      record_pk_column,
      user_id_columns,
      json_metadata,
    };
  }

  #[inline]
  pub fn name(&self) -> &str {
    return &self.schema.name;
  }

  #[inline]
  pub fn column_index_by_name(&self, key: &str) -> Option<usize> {
    return self.name_to_index.get(key).copied();
  }

  #[inline]
  pub fn column_by_name(&self, key: &str) -> Option<(usize, &Column)> {
    let index = self.column_index_by_name(key)?;
    return Some((index, &self.schema.columns[index]));
  }
}

/// A data class describing a sqlite View and future, additional meta data useful for TrailBase.
#[derive(Debug, Clone)]
pub struct ViewMetadata {
  pub schema: View,

  name_to_index: HashMap<String, usize>,
  record_pk_column: Option<usize>,
  json_metadata: Option<JsonMetadata>,
}

impl ViewMetadata {
  /// Build a new ViewMetadata instance containing TrailBase/RecordApi specific information.
  ///
  /// NOTE: The list of all tables is needed only to extract interger/UUIDv7 pk columns for foreign
  /// key relationships.
  pub fn new(view: View, tables: &[Table]) -> Self {
    let name_to_index = if let Some(ref columns) = view.columns {
      HashMap::<String, usize>::from_iter(
        columns
          .iter()
          .enumerate()
          .map(|(index, col)| (col.name.clone(), index)),
      )
    } else {
      HashMap::<String, usize>::new()
    };

    let record_pk_column = view
      .columns
      .as_ref()
      .and_then(|c| find_record_pk_column_index(c, tables));
    let json_metadata = JsonMetadata::from_view(&view);

    return ViewMetadata {
      schema: view,
      name_to_index,
      record_pk_column,
      json_metadata,
    };
  }

  #[inline]
  pub fn name(&self) -> &str {
    &self.schema.name
  }

  #[inline]
  pub fn column_index_by_name(&self, key: &str) -> Option<usize> {
    self.name_to_index.get(key).copied()
  }

  #[inline]
  pub fn column_by_name(&self, key: &str) -> Option<(usize, &Column)> {
    let index = self.column_index_by_name(key)?;
    let cols = self.schema.columns.as_ref()?;
    return Some((index, &cols[index]));
  }
}

pub trait TableOrViewMetadata {
  fn record_pk_column(&self) -> Option<(usize, &Column)>;
  fn json_metadata(&self) -> Option<&JsonMetadata>;
  fn columns(&self) -> Option<&[Column]>;
}

impl TableOrViewMetadata for TableMetadata {
  fn columns(&self) -> Option<&[Column]> {
    return Some(&self.schema.columns);
  }

  fn json_metadata(&self) -> Option<&JsonMetadata> {
    return Some(&self.json_metadata);
  }

  fn record_pk_column(&self) -> Option<(usize, &Column)> {
    let index = self.record_pk_column?;
    return self.schema.columns.get(index).map(|c| (index, c));
  }
}

impl TableOrViewMetadata for ViewMetadata {
  fn columns(&self) -> Option<&[Column]> {
    return self.schema.columns.as_deref();
  }

  fn json_metadata(&self) -> Option<&JsonMetadata> {
    return self.json_metadata.as_ref();
  }

  fn record_pk_column(&self) -> Option<(usize, &Column)> {
    let Some(columns) = &self.schema.columns else {
      return None;
    };
    let index = self.record_pk_column?;
    return columns.get(index).map(|c| (index, c));
  }
}

fn build_json_metadata(col: &Column) -> Option<JsonColumnMetadata> {
  for opt in &col.options {
    match extract_json_metadata(opt) {
      Ok(maybe) => {
        if let Some(jm) = maybe {
          return Some(jm);
        }
      }
      Err(err) => {
        error!("Failed to get JSON schema: {err}");
      }
    }
  }
  None
}

pub fn extract_json_metadata(
  opt: &ColumnOption,
) -> Result<Option<JsonColumnMetadata>, JsonSchemaError> {
  let ColumnOption::Check(check) = opt else {
    return Ok(None);
  };

  lazy_static! {
    static ref SCHEMA_RE: Regex =
      Regex::new(r#"(?smR)jsonschema\s*\(\s*[\['"](?<name>.*)[\]'"]\s*,.+?\)"#)
        .expect("infallible");
    static ref MATCHES_RE: Regex =
      Regex::new(r"(?smR)jsonschema_matches\s*\(.+?(?<pattern>\{.*\}).+?\)").expect("infallible");
  }

  if let Some(cap) = SCHEMA_RE.captures(check) {
    let name = &cap["name"];
    let Some(_schema) = crate::registry::get_schema(name) else {
      let schemas: Vec<String> = crate::registry::get_schemas()
        .iter()
        .map(|s| s.name.clone())
        .collect();
      return Err(JsonSchemaError::NotFound(format!(
        "Json schema {name} not found in: {schemas:?}"
      )));
    };

    return Ok(Some(JsonColumnMetadata::SchemaName(name.to_string())));
  }

  if let Some(cap) = MATCHES_RE.captures(check) {
    let pattern = &cap["pattern"];
    let value = serde_json::from_str::<serde_json::Value>(pattern)
      .map_err(|err| JsonSchemaError::JsonSerialization(Arc::new(err)))?;
    return Ok(Some(JsonColumnMetadata::Pattern(value)));
  }

  return Ok(None);
}

pub fn find_file_column_indexes(json_column_metadata: &[Option<JsonColumnMetadata>]) -> Vec<usize> {
  let mut indexes: Vec<usize> = vec![];

  for (index, column) in json_column_metadata.iter().enumerate() {
    if let Some(metadata) = column {
      match metadata {
        JsonColumnMetadata::SchemaName(name) if name == "std.FileUpload" => {
          indexes.push(index);
        }
        JsonColumnMetadata::SchemaName(name) if name == "std.FileUploads" => {
          indexes.push(index);
        }
        _ => {}
      };
    }
  }

  return indexes;
}

pub fn find_user_id_foreign_key_columns(columns: &[Column], user_table_name: &str) -> Vec<usize> {
  let mut indexes: Vec<usize> = vec![];
  for (index, col) in columns.iter().enumerate() {
    for opt in &col.options {
      if let ColumnOption::ForeignKey {
        foreign_table,
        referred_columns,
        ..
      } = opt
      {
        if foreign_table == user_table_name
          && referred_columns.len() == 1
          && referred_columns[0] == "id"
        {
          indexes.push(index);
        }
      }
    }
  }
  return indexes;
}

pub(crate) fn find_pk_column_index(columns: &[Column]) -> Option<usize> {
  return columns.iter().position(|col| {
    for opt in &col.options {
      if let ColumnOption::Unique { is_primary, .. } = opt {
        return *is_primary;
      }
    }
    return false;
  });
}

/// Finds suitable Integer or UUIDv7 primary key columns, if present.
///
/// Cursors require certain properties like a stable, time-sortable primary key.
fn find_record_pk_column_index(columns: &[Column], tables: &[Table]) -> Option<usize> {
  let index = find_pk_column_index(columns)?;
  let column = &columns[index];

  if column.data_type == ColumnDataType::Integer {
    // TODO: We should detect the "integer pk" desc case and at least warn:
    // https://www.sqlite.org/lang_createtable.html#rowid.
    return Some(index);
  }

  for opts in &column.options {
    lazy_static! {
      static ref UUID_V7_RE: Regex = Regex::new(r"^is_uuid_v7\s*\(").expect("infallible");
    }

    match &opts {
      // Check if the referenced column is a uuidv7 column.
      ColumnOption::ForeignKey {
        foreign_table,
        referred_columns,
        ..
      } => {
        let Some(referred_table) = tables.iter().find(|t| t.name == *foreign_table) else {
          error!("Failed to get foreign key schema for {foreign_table}");
          continue;
        };

        if referred_columns.len() != 1 {
          return None;
        }
        let referred_column = &referred_columns[0];

        let col = referred_table
          .columns
          .iter()
          .find(|c| c.name == *referred_column)?;

        let mut is_pk = false;
        for opt in &col.options {
          match opt {
            ColumnOption::Check(expr) if UUID_V7_RE.is_match(expr) => {
              return Some(index);
            }
            ColumnOption::Unique { is_primary, .. } if *is_primary => {
              is_pk = true;
            }
            _ => {}
          }
        }

        if is_pk && col.data_type == ColumnDataType::Integer {
          return Some(index);
        }

        return None;
      }
      ColumnOption::Check(expr) if UUID_V7_RE.is_match(expr) => {
        return Some(index);
      }
      _ => {}
    }
  }

  return None;
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::sqlite::{Table, sqlite3_parse_into_statement};

  #[test]
  fn test_parse_create_view() {
    let table_name = "table_name";
    let table_sql = format!(
      r#"
      CREATE TABLE {table_name} (
          id                           BLOB PRIMARY KEY NOT NULL CHECK(is_uuid_v7(id)) DEFAULT (uuid_v7()),
          col0                         TEXT NOT NULL DEFAULT '',
          col1                         BLOB NOT NULL,
          hidden                       INTEGER DEFAULT 42
      ) STRICT;"#
    );

    let create_table_statement = sqlite3_parse_into_statement(&table_sql).unwrap().unwrap();

    let table: Table = create_table_statement.try_into().unwrap();

    {
      let metadata = TableMetadata::new(table.clone(), &[table.clone()], "_user");

      assert_eq!(table_name, metadata.name());
      assert_eq!("col1", metadata.columns().unwrap()[2].name);
      assert_eq!(1, *metadata.name_to_index.get("col0").unwrap());
    }

    {
      let view_name = "view_name";
      let query = format!("SELECT col0, col1 FROM {table_name}");
      let view_sql = format!("CREATE VIEW {view_name} AS {query}");
      let create_view_statement = sqlite3_parse_into_statement(&view_sql).unwrap().unwrap();

      let table_view = View::from(create_view_statement, &[table.clone()]).unwrap();

      assert_eq!(table_view.name, view_name);
      assert_eq!(table_view.query, query);
      assert_eq!(table_view.temporary, false);

      let view_columns = table_view.columns.as_ref().unwrap();

      assert_eq!(view_columns.len(), 2);
      assert_eq!(view_columns[0].name, "col0");
      assert_eq!(view_columns[0].data_type, ColumnDataType::Text);

      assert_eq!(view_columns[1].name, "col1");
      assert_eq!(view_columns[1].data_type, ColumnDataType::Blob);

      let view_metadata = ViewMetadata::new(table_view, &[table.clone()]);

      assert!(view_metadata.record_pk_column().is_none());
      assert_eq!(view_metadata.columns().as_ref().unwrap().len(), 2);
    }

    {
      let view_name = "view_name";
      let query = format!("SELECT id, col0, col1 FROM {table_name}");
      let view_sql = format!("CREATE VIEW {view_name} AS {query}");
      let create_view_statement = sqlite3_parse_into_statement(&view_sql).unwrap().unwrap();

      let table_view = View::from(create_view_statement, &[table.clone()]).unwrap();

      assert_eq!(table_view.name, view_name);
      assert_eq!(table_view.query, query);
      assert_eq!(table_view.temporary, false);

      let view_metadata = ViewMetadata::new(table_view, &[table.clone()]);

      let uuidv7_col = view_metadata.record_pk_column().unwrap();
      let columns = view_metadata.columns().unwrap();
      assert_eq!(columns.len(), 3);
      assert_eq!(columns[uuidv7_col.0].name, "id");
    }
  }
}

use lazy_static::lazy_static;
use log::*;
use std::borrow::Cow;
use std::collections::HashMap;
use thiserror::Error;
use trailbase_schema::sqlite::Column;

use crate::records::params::{json_string_to_value, prefix_colon};
use crate::util::b64_to_id;

#[derive(Debug, Error)]
pub enum WhereClauseError {
  #[error("Parse error: {0}")]
  Parse(String),
  #[error("Base64 decoding error: {0}")]
  Base64Decode(#[from] base64::DecodeError),
  #[error("Not implemented error: {0}")]
  NotImplemented(String),
  #[error("Unrecognized param error: {0}")]
  UnrecognizedParam(String),
}

// Syntax: ?key[gte]=value&key[lte]=value
#[derive(Default, Debug, PartialEq)]
pub struct QueryParam {
  pub value: String,
  /// Qualifier or operation such as "greater-than";
  pub qualifier: Option<Qualifier>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Qualifier {
  Not,
  Equal,
  NotEqual,
  GreaterThanEqual,
  GreaterThan,
  LessThanEqual,
  LessThan,
  Like,
  Regexp,
}

impl Qualifier {
  fn from(qualifier: Option<&str>) -> Option<Self> {
    return match qualifier {
      Some("gte") => Some(Self::GreaterThanEqual),
      Some("gt") => Some(Self::GreaterThan),
      Some("lte") => Some(Self::LessThanEqual),
      Some("lt") => Some(Self::LessThan),
      Some("not") => Some(Self::Not),
      Some("ne") => Some(Self::NotEqual),
      Some("like") => Some(Self::Like),
      Some("re") => Some(Self::Regexp),
      None => Some(Self::Equal),
      _ => None,
    };
  }

  fn to_sql(self) -> &'static str {
    return match self {
      Self::GreaterThanEqual => ">=",
      Self::GreaterThan => ">",
      Self::LessThanEqual => "<=",
      Self::LessThan => "<",
      Self::Not => "<>",
      Self::NotEqual => "<>",
      Self::Like => "LIKE",
      Self::Regexp => "REGEXP",
      Self::Equal => "=",
    };
  }
}

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum Order {
  Ascending,
  Descending,
}

#[derive(Debug, PartialEq)]
pub enum Cursor {
  Blob(Vec<u8>),
  Integer(i64),
}

impl Cursor {
  fn parse(value: &str) -> Option<Cursor> {
    if let Ok(id) = b64_to_id(value) {
      return Some(Cursor::Blob(id.into()));
    }

    if let Ok(num) = value.parse::<i64>() {
      return Some(Cursor::Integer(num));
    }

    return None;
  }
}

impl From<Cursor> for rusqlite::types::Value {
  fn from(cursor: Cursor) -> Self {
    return match cursor {
      Cursor::Blob(v) => Self::Blob(v),
      Cursor::Integer(v) => Self::Integer(v),
    };
  }
}

#[derive(Default, Debug)]
pub struct QueryParseResult {
  // Pagination parameters.
  pub limit: Option<usize>,
  pub cursor: Option<Cursor>,
  pub offset: Option<usize>,
  pub count: Option<bool>,
  pub expand: Option<Vec<String>>,

  // Ordering. It's a vector for &order=-col0,+col1,col2
  pub order: Option<Vec<(String, Order)>>,

  // Map from filter params to filter value. It's a vector in cases like
  // "col0[gte]=2&col0[lte]=10".
  pub params: Option<HashMap<String, Vec<QueryParam>>>,
}

pub fn limit_or_default(limit: Option<usize>) -> Result<usize, &'static str> {
  const DEFAULT_LIMIT: usize = 50;
  const MAX_LIMIT: usize = 256;

  if let Some(limit) = limit {
    if limit > MAX_LIMIT {
      return Err("limit exceeds max limit of 256");
    }
    return Ok(limit);
  }
  return Ok(limit.unwrap_or(DEFAULT_LIMIT));
}

fn parse_bool(s: &str) -> Option<bool> {
  return match s {
    "TRUE" | "true" | "1" => Some(true),
    "FALSE" | "false" | "0" => Some(true),
    _ => None,
  };
}

#[inline]
fn sanitize_column_name(name: &str) -> bool {
  // Assuming that all uses are quoted correctly, it should be enough to discard names containing
  // (", ', `, [, ]), however we're conservative here.
  return name
    .chars()
    .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_');
}

/// Parses out list-related query params including pagination (limit, cursort), order, and filters.
///
/// An example query may look like:
///  ?cursor=[0:16]&limit=50&order=price,-date&price[lte]=100&date[gte]=<timestamp>.
pub fn parse_and_sanitize_query(query: Option<&str>) -> Result<QueryParseResult, String> {
  let mut result: QueryParseResult = Default::default();
  let Some(query) = query else {
    return Ok(result);
  };

  if query.is_empty() {
    return Ok(result);
  }

  for (key, value) in form_urlencoded::parse(query.as_bytes()) {
    match key.as_ref() {
      "limit" => result.limit = value.parse::<usize>().ok(),
      "cursor" => result.cursor = Cursor::parse(value.as_ref()),
      "offset" => result.offset = value.parse::<usize>().ok(),
      "count" => result.count = parse_bool(&value),
      "expand" => {
        let column_names = value
          .split(",")
          .map(|s| {
            if !sanitize_column_name(s) {
              return Err(s);
            }

            return Ok(s.to_string());
          })
          .collect::<Result<Vec<_>, _>>()?;

        if !column_names.is_empty() {
          result.expand = Some(column_names);
        }
      }
      "order" => {
        let col_order = value
          .split(",")
          .map(|v| {
            let col_order = match v.trim() {
              x if x.starts_with("-") => (v[1..].to_string(), Order::Descending),
              x if x.starts_with("+") => (v[1..].to_string(), Order::Ascending),
              x => (x.to_string(), Order::Ascending),
            };

            if !sanitize_column_name(&col_order.0) {
              return Err(col_order.0);
            }

            return Ok(col_order);
          })
          .collect::<Result<Vec<_>, _>>()?;

        if !col_order.is_empty() {
          result.order = Some(col_order);
        }
      }
      key => {
        // Key didn't match any of the predefined list operations (limit, cursor, order, ...), we
        // thus assume it's a column filter. We try to split any qualifier/operation, e.g.
        // column[op]=value.
        let Some((k, maybe_op)) = split_key_into_col_and_op(key) else {
          return Err(key.to_string());
        };

        if !sanitize_column_name(k) {
          return Err(key.to_string());
        }

        if value.is_empty() {
          return Err(key.to_string());
        }

        let query_param = QueryParam {
          value: value.to_string(),
          qualifier: Qualifier::from(maybe_op),
        };

        let params = result.params.get_or_insert_default();
        if let Some(v) = params.get_mut(k) {
          v.push(query_param)
        } else {
          params.insert(k.to_string(), vec![query_param]);
        }
      }
    }
  }

  return Ok(result);
}

#[derive(Debug, Clone)]
pub struct WhereClause {
  pub clause: String,
  pub params: Vec<(Cow<'static, str>, trailbase_sqlite::Value)>,
}

pub fn build_filter_where_clause(
  table_name: &str,
  columns: &[Column],
  filter_params: Option<HashMap<String, Vec<QueryParam>>>,
) -> Result<WhereClause, WhereClauseError> {
  let mut where_clauses = Vec::<String>::with_capacity(16);
  let mut params = Vec::<(Cow<'static, str>, trailbase_sqlite::Value)>::with_capacity(16);

  if let Some(filter_params) = filter_params {
    for (column_name, query_params) in filter_params {
      if column_name.starts_with("_") {
        return Err(WhereClauseError::UnrecognizedParam(format!(
          "Invalid parameter: {column_name}"
        )));
      }

      // IMPORTANT: We only include parameters with known columns to avoid building an invalid
      // query early and forbid injections.
      let Some(col) = columns.iter().find(|c| c.name == column_name) else {
        return Err(WhereClauseError::UnrecognizedParam(format!(
          "Unrecognized parameter: {column_name}"
        )));
      };

      for query_param in query_params {
        let Some(op) = query_param.qualifier.map(|q| q.to_sql()) else {
          info!("No op for: {column_name}={query_param:?}");
          continue;
        };

        match json_string_to_value(col.data_type, query_param.value) {
          Ok(value) => {
            where_clauses.push(format!(
              r#"{table_name}."{column_name}" {op} :{column_name}"#
            ));
            params.push((prefix_colon(&column_name).into(), value));
          }
          Err(err) => debug!("Parameter conversion for {column_name} failed: {err}"),
        };
      }
    }
  }

  let clause = match where_clauses.len() {
    0 => "TRUE".to_string(),
    _ => where_clauses.join(" AND "),
  };

  return Ok(WhereClause { clause, params });
}

fn split_key_into_col_and_op(key: &str) -> Option<(&str, Option<&str>)> {
  let Some(captures) = QUALIFIER_REGEX.captures(key) else {
    // Regex didn't match, i.e. key has invalid format.
    return None;
  };

  let Some(k) = captures.name("key") else {
    // No "key" component, i.e. key has invalid format.
    return None;
  };

  return Some((k.as_str(), captures.name("qualifier").map(|c| c.as_str())));
}

lazy_static! {
  /// Regex that splits the key part of "column[op]=value", i.e. column & op.
  static ref QUALIFIER_REGEX: regex::Regex =
    regex::Regex::new(r"^(?<key>\w*)(?:\[(?<qualifier>\w+)\])?$").expect("infallible");
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::util::id_to_b64;
  use crate::util::urlencode;

  #[test]
  fn test_op_splitting_regex() {
    assert_eq!(split_key_into_col_and_op("o82@!&#"), None);
    assert_eq!(split_key_into_col_and_op("a b"), None);

    // Check valid column names
    assert_eq!(split_key_into_col_and_op("foo"), Some(("foo", None)));
    assert_eq!(split_key_into_col_and_op("_foo"), Some(("_foo", None)));

    // Check with ops
    assert_eq!(
      split_key_into_col_and_op("_foo[gte]"),
      Some(("_foo", Some("gte")))
    );
    assert_eq!(split_key_into_col_and_op("_foo[$!]"), None);
  }

  #[test]
  fn test_query_parsing() {
    assert!(parse_and_sanitize_query(None).is_ok());
    assert!(parse_and_sanitize_query(Some("")).is_ok());

    {
      let cursor: [u8; 16] = [5; 16];
      // Note that "+" is encoded as %2b, otherwise it's interpreted as a space. That's barely an
      // inconvenience since + is implied and "-" is fine, so there's no real reason to supply "+"
      // explicitly.
      let query = format!(
        "limit=10&cursor={cursor}&order=%2bcol0,-col1,col2",
        cursor = id_to_b64(&cursor)
      );
      let result = parse_and_sanitize_query(Some(&query)).unwrap();

      assert_eq!(result.limit, Some(10));
      assert_eq!(result.cursor, Some(Cursor::Blob(cursor.to_vec())));
      assert_eq!(
        result.order.unwrap(),
        vec![
          ("col0".to_string(), Order::Ascending),
          ("col1".to_string(), Order::Descending),
          ("col2".to_string(), Order::Ascending),
        ]
      );
    }

    {
      let query = Some("baz=23&bar[like]=foo");
      let result = parse_and_sanitize_query(query).unwrap();

      assert_eq!(
        result.params.as_ref().unwrap().get("baz").unwrap(),
        &vec![QueryParam {
          value: "23".to_string(),
          qualifier: Some(Qualifier::Equal),
        }]
      );
      assert_eq!(
        result.params.as_ref().unwrap().get("bar").unwrap(),
        &vec![QueryParam {
          value: "foo".to_string(),
          qualifier: Some(Qualifier::Like),
        }]
      );
    }

    {
      // foo,bar is an invalid key.
      let query = Some("baz=23&foo,bar&foo_bar");
      assert_eq!(
        parse_and_sanitize_query(query).err(),
        Some("foo,bar".to_string())
      );

      let query = Some("baz=23&foo_bar");
      assert_eq!(
        parse_and_sanitize_query(query).err(),
        Some("foo_bar".to_string())
      );
    }

    {
      // Check whitespaces
      let query = Some("foo=a+b&bar=a%20b");
      let result = parse_and_sanitize_query(query).unwrap();

      assert_eq!(
        result.params.as_ref().unwrap().get("foo").unwrap(),
        &vec![QueryParam {
          value: "a b".to_string(),
          qualifier: Some(Qualifier::Equal),
        }]
      );
      assert_eq!(
        result.params.as_ref().unwrap().get("bar").unwrap(),
        &vec![QueryParam {
          value: "a b".to_string(),
          qualifier: Some(Qualifier::Equal),
        }]
      );
    }

    {
      let query = Some("col_0[gte]=10&col_0[lte]=100");
      let result = parse_and_sanitize_query(query).unwrap();

      assert_eq!(
        result.params.as_ref().unwrap().get("col_0"),
        Some(vec![
          QueryParam {
            value: "10".to_string(),
            qualifier: Some(Qualifier::GreaterThanEqual),
          },
          QueryParam {
            value: "100".to_string(),
            qualifier: Some(Qualifier::LessThanEqual),
          },
        ])
        .as_ref(),
        "{:?}",
        result.params
      );
    }

    {
      // Test both encodings: "+" and %20 for " ".
      let value = "with+white%20spaces";
      let query = Some(format!("text={value}"));
      let result = parse_and_sanitize_query(query.as_deref()).unwrap();

      assert_eq!(
        result.params.as_ref().unwrap().get("text"),
        Some(vec![QueryParam {
          value: "with white spaces".to_string(),
          qualifier: Some(Qualifier::Equal),
        },])
        .as_ref(),
        "{:?}",
        result.params
      );
    }

    {
      // Sanitizing
      assert!(
        parse_and_sanitize_query(Some(&format!("order={}", urlencode("col'; inject")))).is_err()
      );
      assert!(
        parse_and_sanitize_query(Some(&format!("expand={}", urlencode("col'; inject")))).is_err()
      );
      assert!(parse_and_sanitize_query(Some(&urlencode("col'; inject"))).is_err());
    }
  }
}

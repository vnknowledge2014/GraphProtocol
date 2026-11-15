//! Lock-free, lossless columnar table representation for Extension X-1 (`GQL-003`).
//!
//! `spec/02-gap-ql.md` §12 defines the optional `@format(as: COLUMNAR)` payload layout
//! `{"columns":[...],"rows":[...]}` and defers wire activation pending `WP-M7-05` GAP-Bench
//! evidence (`docs/decisions/x1-columnar.md`). This module provides the pure, lossless
//! encoder/decoder codec without activating any wire directive on `1.0-RC`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// Lossless columnar representation of a homogeneous JSON object list (`GQL-003`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnarTable {
    /// Deterministically sorted column names corresponding to each cell position in `rows`.
    pub columns: Vec<String>,
    /// Row values aligned positionally with `columns`.
    pub rows: Vec<Vec<Value>>,
}

/// Measured compact JSON byte comparison between row-oriented objects and [`ColumnarTable`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PayloadSavings {
    /// Serialized compact JSON byte length of the row-oriented object array.
    pub row_json_bytes: usize,
    /// Serialized compact JSON byte length of the equivalent [`ColumnarTable`].
    pub columnar_json_bytes: usize,
    /// Saturated reduction ratio `(row_json_bytes - columnar_json_bytes) / row_json_bytes`.
    pub reduction_ratio: f64,
}

/// Errors returned by columnar encoding, decoding, or serialization helpers.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ColumnarError {
    /// An element in the input slice was not a JSON object.
    #[error("columnar encoding requires JSON object items (index {index} is non-object)")]
    NonObjectItem {
        /// Zero-based index of the invalid element.
        index: usize,
    },
    /// Input objects do not share an identical column key set (lossless encoding requires uniformity).
    #[error(
        "columnar encoding requires identical key sets across objects (mismatch at index {index} for column `{column}`)"
    )]
    HeterogeneousKeys {
        /// Zero-based index of the mismatched object.
        index: usize,
        /// Column name missing or extra at `index`.
        column: String,
    },
    /// A [`ColumnarTable`] declares duplicate column names.
    #[error("duplicate column name `{column}` in columnar table")]
    DuplicateColumn {
        /// Duplicated column identifier.
        column: String,
    },
    /// A row width does not match `columns.len()`.
    #[error("columnar row {index} width mismatch: expected {expected} cells, got {actual}")]
    RowWidthMismatch {
        /// Zero-based index of the malformed row.
        index: usize,
        /// Declared column count.
        expected: usize,
        /// Observed row length.
        actual: usize,
    },
    /// JSON serialization failed.
    #[error("failed to serialize JSON for columnar payload comparison: {message}")]
    Serialize {
        /// Underlying serializer message.
        message: String,
    },
}

impl ColumnarTable {
    /// Construct a validated [`ColumnarTable`] from pre-built column names and rows.
    ///
    /// # Errors
    /// Returns [`ColumnarError::DuplicateColumn`] if any column name repeats, or
    /// [`ColumnarError::RowWidthMismatch`] if any row length differs from `columns.len()`.
    pub fn from_parts(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Result<Self, ColumnarError> {
        let mut seen = BTreeSet::new();
        for col in &columns {
            if !seen.insert(col.as_str()) {
                return Err(ColumnarError::DuplicateColumn {
                    column: col.clone(),
                });
            }
        }
        let expected = columns.len();
        for (index, row) in rows.iter().enumerate() {
            if row.len() != expected {
                return Err(ColumnarError::RowWidthMismatch {
                    index,
                    expected,
                    actual: row.len(),
                });
            }
        }
        Ok(Self { columns, rows })
    }

    /// Encode a homogeneous slice of JSON objects into a [`ColumnarTable`] with sorted columns.
    ///
    /// Returns an empty table for an empty input slice. Rejects non-object items and objects
    /// whose key set differs from element `0` so [`Self::to_objects`] is strictly lossless.
    ///
    /// # Errors
    /// Returns [`ColumnarError::NonObjectItem`] or [`ColumnarError::HeterogeneousKeys`].
    pub fn from_homogeneous_objects(items: &[Value]) -> Result<Self, ColumnarError> {
        let Some(first) = items.first() else {
            return Ok(Self::default());
        };
        let Some(first_map) = first.as_object() else {
            return Err(ColumnarError::NonObjectItem { index: 0 });
        };

        let mut columns: Vec<String> = first_map.keys().cloned().collect();
        columns.sort();

        let mut rows = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                return Err(ColumnarError::NonObjectItem { index });
            };
            if obj.len() != columns.len() {
                if let Some(missing) = columns.iter().find(|k| !obj.contains_key(*k)) {
                    return Err(ColumnarError::HeterogeneousKeys {
                        index,
                        column: missing.clone(),
                    });
                }
                if let Some(extra) = obj.keys().find(|k| columns.binary_search(k).is_err()) {
                    return Err(ColumnarError::HeterogeneousKeys {
                        index,
                        column: extra.clone(),
                    });
                }
            }
            let mut row = Vec::with_capacity(columns.len());
            for col in &columns {
                let Some(cell) = obj.get(col) else {
                    return Err(ColumnarError::HeterogeneousKeys {
                        index,
                        column: col.clone(),
                    });
                };
                row.push(cell.clone());
            }
            rows.push(row);
        }

        Ok(Self { columns, rows })
    }

    /// Losslessly expand this [`ColumnarTable`] back into row-oriented JSON objects.
    ///
    /// # Errors
    /// Returns [`ColumnarError::DuplicateColumn`] or [`ColumnarError::RowWidthMismatch`] if the
    /// table violates structural invariants.
    pub fn to_objects(&self) -> Result<Vec<Value>, ColumnarError> {
        let mut seen = BTreeSet::new();
        for col in &self.columns {
            if !seen.insert(col.as_str()) {
                return Err(ColumnarError::DuplicateColumn {
                    column: col.clone(),
                });
            }
        }
        let expected = self.columns.len();
        let mut out = Vec::with_capacity(self.rows.len());
        for (index, row) in self.rows.iter().enumerate() {
            if row.len() != expected {
                return Err(ColumnarError::RowWidthMismatch {
                    index,
                    expected,
                    actual: row.len(),
                });
            }
            let mut obj = Map::with_capacity(expected);
            for (col, cell) in self.columns.iter().zip(row.iter()) {
                obj.insert(col.clone(), cell.clone());
            }
            out.push(Value::Object(obj));
        }
        Ok(out)
    }

    /// Compare compact JSON wire bytes between `objects` and their [`ColumnarTable`] encoding.
    ///
    /// # Errors
    /// Returns [`ColumnarError`] if `objects` is non-homogeneous or serialization fails.
    pub fn measure_compact_json_savings(
        objects: &[Value],
    ) -> Result<(Self, PayloadSavings), ColumnarError> {
        let table = Self::from_homogeneous_objects(objects)?;
        let row_bytes = serde_json::to_vec(objects)
            .map_err(|err| ColumnarError::Serialize {
                message: err.to_string(),
            })?
            .len();
        let columnar_bytes = serde_json::to_vec(&table)
            .map_err(|err| ColumnarError::Serialize {
                message: err.to_string(),
            })?
            .len();

        let ratio = if row_bytes == 0 || columnar_bytes >= row_bytes {
            0.0
        } else {
            (row_bytes - columnar_bytes) as f64 / row_bytes as f64
        };

        Ok((
            table,
            PayloadSavings {
                row_json_bytes: row_bytes,
                columnar_json_bytes: columnar_bytes,
                reduction_ratio: ratio,
            },
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::{ColumnarError, ColumnarTable};

    // spec: GQL-003
    #[test]
    fn x1_columnar_round_trips_homogeneous_objects_losslessly() {
        let input = vec![
            json!({"id": "ord-1", "total": 10.5, "paid": true}),
            json!({"paid": false, "total": 42, "id": "ord-2"}),
            json!({"total": null, "id": "ord-3", "paid": false}),
        ];

        let table = ColumnarTable::from_homogeneous_objects(&input).unwrap();
        assert_eq!(table.columns, vec!["id", "paid", "total"]);
        assert_eq!(
            table.rows,
            vec![
                vec![json!("ord-1"), json!(true), json!(10.5)],
                vec![json!("ord-2"), json!(false), json!(42)],
                vec![json!("ord-3"), json!(false), json!(null)],
            ]
        );

        let wire = serde_json::to_value(&table).unwrap();
        assert_eq!(
            wire,
            json!({
                "columns": ["id", "paid", "total"],
                "rows": [
                    ["ord-1", true, 10.5],
                    ["ord-2", false, 42],
                    ["ord-3", false, null]
                ]
            })
        );

        let expanded = table.to_objects().unwrap();
        assert_eq!(expanded, input);
    }

    // spec: GQL-003
    #[test]
    fn x1_columnar_handles_empty_lists_and_rejects_heterogeneous_inputs() {
        let empty = ColumnarTable::from_homogeneous_objects(&[]).unwrap();
        assert!(empty.columns.is_empty());
        assert!(empty.rows.is_empty());
        assert!(empty.to_objects().unwrap().is_empty());

        let non_obj = ColumnarTable::from_homogeneous_objects(&[json!({"id": 1}), json!(2)]);
        assert_eq!(non_obj, Err(ColumnarError::NonObjectItem { index: 1 }));

        let mismatch = ColumnarTable::from_homogeneous_objects(&[
            json!({"id": 1, "sku": "A"}),
            json!({"id": 2, "status": "OPEN"}),
        ]);
        assert_eq!(
            mismatch,
            Err(ColumnarError::HeterogeneousKeys {
                index: 1,
                column: "sku".to_owned(),
            })
        );

        let dup = ColumnarTable::from_parts(
            vec!["id".to_owned(), "id".to_owned()],
            vec![vec![json!(1), json!(2)]],
        );
        assert_eq!(
            dup,
            Err(ColumnarError::DuplicateColumn {
                column: "id".to_owned(),
            })
        );

        let width_err = ColumnarTable::from_parts(
            vec!["id".to_owned(), "sku".to_owned()],
            vec![vec![json!(1)]],
        );
        assert_eq!(
            width_err,
            Err(ColumnarError::RowWidthMismatch {
                index: 0,
                expected: 2,
                actual: 1,
            })
        );
    }

    // spec: GQL-003
    #[test]
    fn x1_columnar_reduces_large_homogeneous_list_wire_bytes() {
        let large: Vec<serde_json::Value> = (0..100)
            .map(|i| {
                json!({
                    "id": i,
                    "customer_account_id": format!("acct-{i:04}"),
                    "invoice_currency_code": "USD",
                    "settlement_status": "SETTLED",
                    "total_amount_cents": 1250 + i
                })
            })
            .collect();

        let (_table, savings) = ColumnarTable::measure_compact_json_savings(&large).unwrap();
        assert!(
            savings.columnar_json_bytes < savings.row_json_bytes,
            "expected columnar encoding to save wire bytes on 100-row homogeneous objects"
        );
        assert!(
            savings.reduction_ratio > 0.40,
            "expected > 40% payload compression on synthetic 5-column homogeneous array, got {:.3}",
            savings.reduction_ratio
        );
    }
}

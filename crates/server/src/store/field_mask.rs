//! Applying a proto field mask to a row.
//!
//! An update names the fields it means and leaves the rest alone, so the write
//! is not known until the mask has been read. The obvious implementation — one
//! `UPDATE` per named column, wrapped in a transaction to make the set of them
//! atomic — turns a single logical write into up to a dozen round trips and
//! makes atomicity something the caller has to remember rather than something
//! the statement already is.
//!
//! So the columns are collected first and written once. The transaction goes
//! away with them: one statement is atomic on its own.

use sqlx::AssertSqlSafe;

use super::Store;

/// A value bound into a masked update.
///
/// Only the shapes the columns actually hold. Text is borrowed where the caller
/// has a borrow and owned where it had to build one (an encoded JSON column),
/// which saves a copy of every string on the common path.
#[derive(Debug)]
pub(crate) enum Value<'a> {
    Text(&'a str),
    Owned(String),
    Int(i64),
}

/// One column named by a mask, and what to write to it.
#[derive(Debug)]
pub(crate) struct Field<'a> {
    /// Always a literal at the call site — never a caller-supplied name, which
    /// is what keeps the composed SQL below safe.
    column: &'static str,
    value: Value<'a>,
}

impl<'a> Field<'a> {
    pub(crate) fn text(column: &'static str, value: &'a str) -> Self {
        Self {
            column,
            value: Value::Text(value),
        }
    }

    pub(crate) fn owned(column: &'static str, value: String) -> Self {
        Self {
            column,
            value: Value::Owned(value),
        }
    }

    pub(crate) fn int(column: &'static str, value: i64) -> Self {
        Self {
            column,
            value: Value::Int(value),
        }
    }
}

impl Store {
    /// Writes the masked columns of one row, in one statement.
    ///
    /// A mask that named nothing writes nothing — and issues no statement at
    /// all, rather than opening and committing an empty transaction.
    ///
    /// `table` and every column name are `&'static str` literals from the
    /// caller; the values are bound. That is what makes the composed SQL safe,
    /// and it is why [`Field`] does not accept a runtime column name.
    pub(crate) async fn update_masked(
        &self,
        table: &'static str,
        id: &str,
        fields: &[Field<'_>],
    ) -> Result<(), sqlx::Error> {
        if fields.is_empty() {
            return Ok(());
        }

        let assignments = fields
            .iter()
            .enumerate()
            .map(|(index, field)| format!("{} = ?{}", field.column, index + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE {table} SET {assignments} WHERE id = ?{}",
            fields.len() + 1
        );

        let mut query = sqlx::query(AssertSqlSafe(sql));
        for field in fields {
            query = match &field.value {
                Value::Text(text) => query.bind(*text),
                Value::Owned(text) => query.bind(text.as_str()),
                Value::Int(number) => query.bind(*number),
            };
        }

        query.bind(id).execute(self.pool()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TargetInput;

    #[tokio::test]
    async fn a_mask_that_names_nothing_writes_nothing() {
        let store = Store::open_in_memory().await.expect("open");
        let target = store
            .create_target(&TargetInput {
                name: "web".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("create");

        // An empty field list must not compose `UPDATE targets SET  WHERE ...`.
        store
            .update_masked("targets", &target.id, &[])
            .await
            .expect("no-op");

        let after = store
            .get_target(&target.id)
            .await
            .expect("get")
            .expect("target");
        assert_eq!(after.name, "web");
        assert_eq!(after.host, "10.0.0.1");
    }

    #[tokio::test]
    async fn every_named_column_is_written_in_one_statement() {
        let store = Store::open_in_memory().await.expect("open");
        let target = store
            .create_target(&TargetInput {
                name: "web".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("create");

        store
            .update_masked(
                "targets",
                &target.id,
                &[
                    Field::text("name", "api"),
                    Field::int("port", 2222),
                    Field::owned("labels", r#"{"env":"prod"}"#.to_string()),
                ],
            )
            .await
            .expect("update");

        let after = store
            .get_target(&target.id)
            .await
            .expect("get")
            .expect("target");
        assert_eq!(after.name, "api");
        assert_eq!(after.port, 2222);
        assert_eq!(after.labels.get("env").map(String::as_str), Some("prod"));
        // Columns the mask did not name are untouched.
        assert_eq!(after.host, "10.0.0.1");
    }

    #[tokio::test]
    async fn a_rejected_write_leaves_every_column_as_it_was() {
        let store = Store::open_in_memory().await.expect("open");
        for name in ["web", "api"] {
            store
                .create_target(&TargetInput {
                    name: name.to_string(),
                    host: "10.0.0.1".to_string(),
                    ..Default::default()
                })
                .await
                .expect("create");
        }
        let target = store
            .list_targets("", 10, 0)
            .await
            .expect("list")
            .into_iter()
            .find(|t| t.name == "api")
            .expect("api");

        // One statement, so the unique-name violation cannot leave the port
        // applied — there is no partial state to leave.
        store
            .update_masked(
                "targets",
                &target.id,
                &[Field::text("name", "web"), Field::int("port", 2222)],
            )
            .await
            .expect_err("the name is taken");

        let after = store
            .get_target(&target.id)
            .await
            .expect("get")
            .expect("target");
        assert_eq!(after.name, "api");
        assert_eq!(after.port, 22, "the port must not have been written");
    }
}

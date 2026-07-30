// ---- SPIKE (temporary): does `INSERT INTO ... SELECT` typecheck here? ----
#[allow(dead_code, unused_imports)]
mod fork_spike {
    use super::*;
    use diesel::expression::IntoSql;
    use diesel::sql_types::Text;

    pub async fn copy_messages(
        conn: &mut AsyncPgConnection,
        workspace_id: &str,
        source_id: &str,
        new_id: &str,
        keep: Vec<uuid::Uuid>,
    ) -> Result<usize, diesel::result::Error> {
        diesel::insert_into(messages::table)
            .values(
                messages::table
                    .filter(
                        messages::workspace_id
                            .eq(workspace_id)
                            .and(messages::session_id.eq(source_id))
                            .and(messages::turn_id.eq_any(keep)),
                    )
                    .select((
                        messages::workspace_id,
                        new_id.into_sql::<Text>(),
                        messages::thread_id,
                        messages::seq,
                        messages::message_id,
                        messages::turn_id,
                        messages::role,
                        messages::origin_message_id,
                        messages::origin_call_id,
                        messages::payload,
                    )),
            )
            .into_columns((
                messages::workspace_id,
                messages::session_id,
                messages::thread_id,
                messages::seq,
                messages::message_id,
                messages::turn_id,
                messages::role,
                messages::origin_message_id,
                messages::origin_call_id,
                messages::payload,
            ))
            .execute(conn)
            .await
    }
}

// @generated automatically by Diesel CLI.

diesel::table! {
    messages (workspace_id, session_id, thread_id, seq) {
        workspace_id -> Text,
        session_id -> Text,
        thread_id -> Text,
        seq -> Int4,
        message_id -> Uuid,
        turn_id -> Uuid,
        role -> Text,
        origin_message_id -> Nullable<Uuid>,
        origin_call_id -> Nullable<Text>,
        payload -> Jsonb,
        created_at -> Timestamptz,
    }
}

diesel::table! {
    runtime_snapshots (workspace_id, session_id) {
        workspace_id -> Text,
        session_id -> Text,
        snapshot -> Jsonb,
    }
}

diesel::table! {
    sessions (workspace_id, session_id) {
        workspace_id -> Text,
        session_id -> Text,
        name -> Nullable<Text>,
        model_binding -> Jsonb,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
    }
}

diesel::table! {
    thread_checkpoints (workspace_id, session_id, thread_id) {
        workspace_id -> Text,
        session_id -> Text,
        thread_id -> Text,
        agent_name -> Text,
        parent_thread_id -> Nullable<Text>,
        derivation_key -> Nullable<Text>,
        reply_target -> Nullable<Jsonb>,
        resume_point -> Jsonb,
        suspended_at -> Timestamptz,
        message_count -> Int4,
        pending_approval -> Bool,
    }
}

diesel::table! {
    thread_state (workspace_id, session_id, message_id, kind) {
        workspace_id -> Text,
        session_id -> Text,
        message_id -> Uuid,
        kind -> Text,
        value -> Jsonb,
    }
}

diesel::allow_tables_to_appear_in_same_query!(
    messages,
    runtime_snapshots,
    sessions,
    thread_checkpoints,
    thread_state,
);

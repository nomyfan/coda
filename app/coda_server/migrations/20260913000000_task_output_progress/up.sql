CREATE TABLE task_output_progress (
    workspace_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    consumer TEXT NOT NULL,
    task_id TEXT NOT NULL,
    channel TEXT NOT NULL CHECK (channel IN ('stdout', 'stderr', 'result', 'log')),
    next_offset BIGINT NOT NULL CHECK (next_offset >= 0),
    PRIMARY KEY (workspace_id, session_id, consumer, task_id, channel),
    FOREIGN KEY (workspace_id, session_id) REFERENCES sessions (workspace_id, session_id) ON DELETE CASCADE
);

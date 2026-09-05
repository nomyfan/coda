DROP TABLE aborted_executions;
DROP TABLE task_notice_receipts;
ALTER TABLE thread_checkpoints RENAME COLUMN active_execution TO reply_target;

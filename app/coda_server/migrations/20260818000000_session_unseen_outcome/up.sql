-- Set when a turn settles (completes, aborts, or errors) with nobody attached
-- to the session, so the sidebar can show it happened; cleared on the next
-- attach. Suspensions awaiting approval don't set it — `has_pending_approval`
-- already covers that case.
alter table sessions
    add column unseen_outcome text
        check (unseen_outcome in ('completed', 'failed'));

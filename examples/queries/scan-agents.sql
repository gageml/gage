-- The agent sessions scans ran: which task launched each, how it exited,
-- and how long the conversation was
SELECT a.scan_id, a.scanner, a.task, a.session_id, s.model, s.message_count, a.exit_code
FROM scan_task_agent a
JOIN session s ON s.id = a.session_id
ORDER BY a.scan_id, a.scanner, a.task;

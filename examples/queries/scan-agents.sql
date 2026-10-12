-- The agent sessions scans ran: which task launched each, how it ended,
-- what it cost, and how long the conversation was
SELECT a.scan_id, a.scanner, a.task, a.session_id, s.model, s.message_count,
       a.is_error, a.turns, a.cost_usd
FROM scan_task_agent a
JOIN session s ON s.id = a.session_id
ORDER BY a.scan_id, a.scanner, a.task;

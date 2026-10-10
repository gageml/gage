-- How the newest scan's tasks went, in dispatch order, with working time
SELECT t.scanner, t.task, t.status, t.started, t.stopped, t.worked_ms
FROM scan_task t
WHERE t.scan_id = (SELECT id FROM scan ORDER BY started DESC LIMIT 1)
ORDER BY t.num;

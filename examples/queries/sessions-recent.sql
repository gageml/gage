-- Sessions touched in the last 7 days, newest first, with size and model
SELECT id, project, title, model, message_count, line_count, native_size, modified
FROM session
WHERE modified > now() - INTERVAL '7 days'
ORDER BY modified DESC
LIMIT 20;

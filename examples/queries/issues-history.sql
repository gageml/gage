-- The latest status changes and comments across issues
SELECT e.issue_id, i.title, e.timestamp, e.event, e.from_status, e.to_status,
       e.reason, e.author, substr(e.message, 1, 80) AS message
FROM issue_event e
JOIN issue i ON i.id = e.issue_id
ORDER BY e.timestamp DESC
LIMIT 50;

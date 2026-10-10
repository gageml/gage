-- How much session activity each project has: sessions, lines, and the
-- models used
SELECT project,
       COUNT(*) AS sessions,
       SUM(message_count) AS messages,
       SUM(line_count) AS lines,
       string_agg(DISTINCT model, ', ') AS models,
       MAX(modified) AS last_activity
FROM session
WHERE NOT is_empty
GROUP BY project
ORDER BY last_activity DESC;

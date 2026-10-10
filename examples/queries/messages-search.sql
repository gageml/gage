-- Find the sessions where the user mentioned a term, with the line to open
SELECT m.session_id, s.project, m.line, substr(m.text, 1, 120) AS snippet
FROM message m
JOIN session s ON s.id = m.session_id
WHERE m.type = 'user' AND m.text ILIKE '%clippy%'
ORDER BY m.session_id, m.line
LIMIT 50;

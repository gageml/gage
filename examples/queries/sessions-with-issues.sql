-- Which sessions have unresolved issues against them, most first
SELECT si.session_id, s.project, s.title, COUNT(*) AS issues
FROM session_issue si
JOIN session s ON s.id = si.session_id
JOIN issue i ON i.id = si.issue_id
WHERE i.status <> 'closed'
GROUP BY si.session_id, s.project, s.title
ORDER BY issues DESC;

-- What a session's transcript is made of: entry counts by type and subtype
SELECT session_id, type, subtype, COUNT(*) AS entries
FROM entry
WHERE session_id = (SELECT id FROM session ORDER BY modified DESC LIMIT 1)
GROUP BY session_id, type, subtype
ORDER BY entries DESC;

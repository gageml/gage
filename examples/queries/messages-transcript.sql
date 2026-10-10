-- The conversation of the most recently modified session, in order
SELECT line, type, subtype, substr(text, 1, 200) AS text
FROM message
WHERE session_id = (SELECT id FROM session ORDER BY modified DESC LIMIT 1)
ORDER BY line;

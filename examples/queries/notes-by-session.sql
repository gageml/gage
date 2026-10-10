-- Notes placed on sessions, with the lines they cite and who wrote them
SELECT sn.session_id, s.title, n.name, sn.lines,
       substr(n.text, 1, 120) AS text, n.author, n.created
FROM session_note sn
JOIN note n ON n.id = sn.note_id
JOIN session s ON s.id = sn.session_id
ORDER BY n.created DESC
LIMIT 50;

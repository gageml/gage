-- The notes cited as evidence by unresolved issues, with what they target
SELECT ie.issue_id, i.title, n.id AS note_id, n.name, n.target,
       substr(n.text, 1, 120) AS text
FROM issue_evidence ie
JOIN issue i ON i.id = ie.issue_id
JOIN note n ON n.id = ie.note_id
WHERE i.status <> 'closed'
ORDER BY ie.issue_id, n.created;

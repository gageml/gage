-- Every note kind a scanner declares, who writes it, and how many exist
SELECT d.note_name, d.written_by, COUNT(n.id) AS notes, d.doc
FROM note_doc d
LEFT JOIN note n ON n.name = d.note_name
GROUP BY d.note_name, d.written_by, d.doc
ORDER BY notes DESC, d.note_name;

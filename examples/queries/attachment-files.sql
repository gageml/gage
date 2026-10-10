-- The files inside the claude-config attachments, with their text
SELECT a.name, a.modified, f.path, f.size, substr(f.text, 1, 200) AS text
FROM attachment_file f
JOIN attachment a ON a.id = f.attachment_id
WHERE a.name = 'claude-config'
ORDER BY a.modified DESC, f.path;

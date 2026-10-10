-- Every tag and the object it names, with the object's type
SELECT t.name, t.id,
       CASE WHEN d.id IS NOT NULL THEN 'dataset'
            WHEN s.id IS NOT NULL THEN 'session'
            WHEN sc.id IS NOT NULL THEN 'scan'
            WHEN n.id IS NOT NULL THEN 'note'
            WHEN i.id IS NOT NULL THEN 'issue'
            WHEN a.id IS NOT NULL THEN 'attachment'
       END AS type
FROM tag t
LEFT JOIN dataset d ON d.id = t.id
LEFT JOIN session s ON s.id = t.id
LEFT JOIN scan sc ON sc.id = t.id
LEFT JOIN note n ON n.id = t.id
LEFT JOIN issue i ON i.id = t.id
LEFT JOIN attachment a ON a.id = t.id
ORDER BY t.name;

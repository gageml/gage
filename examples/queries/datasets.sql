-- Datasets with their member counts, attachments, and tags
SELECT d.id, d.created,
       COALESCE(m.sessions, 0) AS sessions,
       COALESCE(a.attachments, 0) AS attachments,
       t.tags
FROM dataset d
LEFT JOIN (SELECT dataset_id, COUNT(*) AS sessions FROM dataset_session
           GROUP BY dataset_id) m ON m.dataset_id = d.id
LEFT JOIN (SELECT dataset_id, COUNT(*) AS attachments FROM dataset_attachment
           GROUP BY dataset_id) a ON a.dataset_id = d.id
LEFT JOIN (SELECT id, string_agg(name, ', ' ORDER BY name) AS tags FROM tag
           GROUP BY id) t ON t.id = d.id
ORDER BY d.created DESC;

-- Attachments in the store: what they hold and how many datasets use them
SELECT a.id, a.name, a.key, a.file_count, a.size, a.modified,
       (SELECT COUNT(*) AS n FROM dataset_attachment da WHERE da.attachment_id = a.id) AS datasets
FROM attachment a
ORDER BY a.modified DESC;

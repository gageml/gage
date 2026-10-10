-- The members of the newest dataset, in dataset order
SELECT ds.session_num, ds.session_id, s.project, s.title, s.message_count
FROM dataset_session ds
JOIN session s ON s.id = ds.session_id
WHERE ds.dataset_id = (SELECT id FROM dataset ORDER BY created DESC LIMIT 1)
ORDER BY ds.session_num;

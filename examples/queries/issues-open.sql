-- Issues awaiting action: pending and open, with how much evidence backs each
SELECT id, status, name, title, evidence_count, author, scan, created
FROM issue
WHERE status IN ('pending', 'open')
ORDER BY created DESC;

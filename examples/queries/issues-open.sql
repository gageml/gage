-- Issues awaiting action: pending and open, with how much evidence backs each
SELECT i.id, i.status, i.name, i.title, COALESCE(e.notes, 0) AS evidence,
       i.author, i.scan, i.created
FROM issue i
LEFT JOIN (SELECT issue_id, COUNT(*) AS notes FROM issue_evidence
           GROUP BY issue_id) e ON e.issue_id = i.id
WHERE i.status IN ('pending', 'open')
ORDER BY i.created DESC;

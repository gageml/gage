-- Recent scans: when they ran, how their tasks ended, and what they produced
SELECT s.id, s.started, s.stopped, s.canceled,
       s.tasks, s.completed, s.failed, s.skipped,
       COALESCE(ss.sessions, 0) AS sessions,
       COALESCE(sn.notes, 0) AS notes,
       COALESCE(sn.carried, 0) AS carried,
       COALESCE(si.issues, 0) AS issues
FROM scan s
LEFT JOIN (SELECT scan_id, COUNT(*) AS sessions FROM scan_session
           GROUP BY scan_id) ss ON ss.scan_id = s.id
LEFT JOIN (SELECT scan_id,
                  SUM(CASE WHEN carried THEN 0 ELSE 1 END) AS notes,
                  SUM(CASE WHEN carried THEN 1 ELSE 0 END) AS carried
           FROM scan_note GROUP BY scan_id) sn ON sn.scan_id = s.id
LEFT JOIN (SELECT scan_id, COUNT(*) AS issues FROM scan_issue
           GROUP BY scan_id) si ON si.scan_id = s.id
ORDER BY s.started DESC
LIMIT 20;

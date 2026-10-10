-- What each scan wrote versus carried forward, by note kind
SELECT sn.scan_id, sn.carried, n.name, COUNT(*) AS notes
FROM scan_note sn
JOIN note n ON n.id = sn.note_id
GROUP BY sn.scan_id, sn.carried, n.name
ORDER BY sn.scan_id, sn.carried, n.name;

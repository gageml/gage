//! Note-to-note relationships. Each row expresses "note `note_id`
//! relates to note `related_to`" with a `relation` noun. `relation`
//! is NOT NULL; pass `""` to mean "infer from `note_id.name`"
//! (the common case). Schema + semantics:
//! see `docs/design/scanner-types.md#note_relation`.

use rusqlite::{Connection, params};

pub fn insert_relation(
    conn: &Connection,
    note_id: &str,
    related_to: &str,
    relation: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO note_relation (note_id, related_to, relation)
         VALUES (?1, ?2, ?3)",
    )?;
    stmt.execute(params![note_id, related_to, relation])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_in_memory;
    use crate::note::{self, Note};
    use crate::target::{NoteTarget, SessionTarget};

    fn make_note(conn: &Connection, id: &str) {
        let note = Note {
            id: id.to_string(),
            author: "test".to_string(),
            created: 1_745_452_800_000,
            modified: None,
            target: NoteTarget::Session(SessionTarget::new("11111111-1111-1111-1111-111111111111")),
            name: format!("test.note.{id}"),
            value: crate::note::NoteValue::from(""),
            explanation: None,
            metadata: None,
        };
        note::insert(conn, &note).unwrap();
    }

    #[test]
    fn insert_and_read_back() {
        let conn = open_db_in_memory().unwrap();
        make_note(&conn, "note-a");
        make_note(&conn, "note-b");

        insert_relation(&conn, "note-a", "note-b", "").unwrap();
        insert_relation(&conn, "note-a", "note-b", "cites").unwrap();

        let count: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_relation WHERE note_id = 'note-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn duplicate_inferred_relation_rejected() {
        let conn = open_db_in_memory().unwrap();
        make_note(&conn, "note-a");
        make_note(&conn, "note-b");

        insert_relation(&conn, "note-a", "note-b", "").unwrap();
        let err = insert_relation(&conn, "note-a", "note-b", "")
            .expect_err("duplicate (note_id, related_to, '') should violate PK");
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
    }
}

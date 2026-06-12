//! Tantivy index over message text. The persistent index is the sole
//! FTS surface: it stores the text field so snippet generation needs
//! no external lookup, and its search returns scores and snippets, not
//! booleans.
//!
//! Schema: `session_id` (STRING|STORED), `line` (u64|STORED|FAST),
//! `type` (STRING|STORED), `subtype` (STRING|STORED),
//! `text` (TEXT|STORED). Tokenizer is the default chain (split on
//! non-alphanumeric, drop tokens over 40 bytes, lowercase). Query
//! parser defaults to AND across bare terms.

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
    Value as _,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, IndexWriter, Score, TantivyDocument, Term};

use crate::{IndexError, Result};

/// Index format version: covers the index schema and tokenizer chain.
/// Bumping it changes the `v{N}` path component.
pub const INDEX_FORMAT_VERSION: u32 = 3;

/// Canonical identifier of the tokenizer chain, recorded in the index
/// manifest. A mismatch with running code triggers an automatic index
/// rebuild.
pub const TOKENIZER_CHAIN: &str = "simple+remove_long:40+lowercase";

const TOKENIZER_NAME: &str = "gage_default";
const MAX_TOKEN_LEN: usize = 40;

/// Default character cap on generated snippets.
pub const DEFAULT_SNIPPET_CHARS: usize = 200;

/// One hit from a `message_text` search.
#[derive(Debug, Clone)]
pub struct Hit {
    pub session_id: String,
    pub line: i64,
    pub type_: Option<String>,
    pub subtype: Option<String>,
    pub score: f32,
    pub snippet: String,
}

fn build_tokenizer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_LEN))
        .filter(LowerCaser)
        .build()
}

fn register_tokenizer(index: &Index) {
    index
        .tokenizers()
        .register(TOKENIZER_NAME, build_tokenizer());
}

fn text_options() -> TextOptions {
    TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored()
}

pub(crate) struct TextIndex {
    index: Index,
    f_session: Field,
    f_line: Field,
    f_type: Field,
    f_subtype: Field,
    f_text: Field,
}

fn index_schema() -> Schema {
    let mut builder = Schema::builder();
    // Raw term so `delete_term` removes a session's documents on
    // re-index.
    builder.add_text_field("session_id", STRING | STORED);
    builder.add_u64_field("line", STORED | FAST);
    builder.add_text_field("type", STRING | STORED);
    builder.add_text_field("subtype", STRING | STORED);
    builder.add_text_field("text", text_options());
    builder.build()
}

impl TextIndex {
    /// Open the index in `dir`, creating it if absent. Errors from a
    /// corrupt or schema-incompatible index surface here; the
    /// reconciler responds by wiping and rebuilding.
    pub(crate) fn open_or_create(dir: &Path) -> Result<Self> {
        let schema = index_schema();
        let index = if dir.join("meta.json").exists() {
            Index::open_in_dir(dir)?
        } else {
            Index::create_in_dir(dir, schema.clone())?
        };
        register_tokenizer(&index);
        let actual = index.schema();
        let f_session = actual.get_field("session_id")?;
        let f_line = actual.get_field("line")?;
        let f_type = actual.get_field("type")?;
        let f_subtype = actual.get_field("subtype")?;
        let f_text = actual.get_field("text")?;
        Ok(Self {
            index,
            f_session,
            f_line,
            f_type,
            f_subtype,
            f_text,
        })
    }

    pub(crate) fn writer(&self) -> Result<IndexWriter> {
        Ok(self.index.writer(64_000_000)?)
    }

    pub(crate) fn delete_session(&self, writer: &IndexWriter, session_id: &str) {
        #[allow(clippy::let_underscore_must_use)]
        let _ = writer.delete_term(Term::from_field_text(self.f_session, session_id));
    }

    pub(crate) fn add_message(
        &self,
        writer: &IndexWriter,
        session_id: &str,
        line: i64,
        type_: Option<&str>,
        subtype: Option<&str>,
        text: &str,
    ) -> Result<()> {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.f_session, session_id);
        doc.add_u64(self.f_line, line as u64);
        if let Some(t) = type_ {
            doc.add_text(self.f_type, t);
        }
        if let Some(s) = subtype {
            doc.add_text(self.f_subtype, s);
        }
        doc.add_text(self.f_text, text);
        writer.add_document(doc)?;
        Ok(())
    }

    /// Run a query against the committed index, returning the top
    /// `limit` hits with BM25 scores and snippets capped at
    /// `snippet_chars`. Matches in snippets are wrapped in guillemets
    /// (`«match»`). Hits come back ordered by score, descending.
    pub(crate) fn search(
        &self,
        query: &str,
        limit: usize,
        snippet_chars: usize,
    ) -> Result<Vec<Hit>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let mut parser = QueryParser::for_index(&self.index, vec![self.f_text]);
        parser.set_conjunction_by_default();
        let query = parser
            .parse_query(query)
            .map_err(|e| IndexError::QueryParse(e.to_string()))?;

        let mut snippet_generator = SnippetGenerator::create(&searcher, &*query, self.f_text)?;
        snippet_generator.set_max_num_chars(snippet_chars);

        let top: Vec<(Score, _)> =
            searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let session_id = doc
                .get_first(self.f_session)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let line = doc
                .get_first(self.f_line)
                .and_then(|v| v.as_u64())
                .unwrap_or_default() as i64;
            let type_ = doc
                .get_first(self.f_type)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let subtype = doc
                .get_first(self.f_subtype)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let snippet = snippet_generator.snippet_from_doc(&doc);
            hits.push(Hit {
                session_id,
                line,
                type_,
                subtype,
                score,
                snippet: format_snippet(&snippet),
            });
        }
        Ok(hits)
    }
}

/// Wrap highlighted ranges in guillemets (`«match»`). Source text
/// often contains markdown `**...**`, so a markdown delimiter would
/// collide; guillemets are vanishingly rare in code and prose,
/// single-codepoint on each side, and a simple regex strips them for
/// consumers that want the plain fragment.
fn format_snippet(snippet: &tantivy::snippet::Snippet) -> String {
    const OPEN: &str = "«";
    const CLOSE: &str = "»";
    let fragment = snippet.fragment();
    let highlights = snippet.highlighted();
    if highlights.is_empty() {
        return fragment.to_string();
    }
    let mut out =
        String::with_capacity(fragment.len() + highlights.len() * (OPEN.len() + CLOSE.len()));
    let mut cursor = 0;
    let mut sorted: Vec<_> = highlights.to_vec();
    sorted.sort_by_key(|r| r.start);
    for range in sorted {
        // Skip overlapping / out-of-order ranges defensively.
        if range.start < cursor || range.end > fragment.len() {
            continue;
        }
        out.push_str(&fragment[cursor..range.start]);
        out.push_str(OPEN);
        out.push_str(&fragment[range.start..range.end]);
        out.push_str(CLOSE);
        cursor = range.end;
    }
    out.push_str(&fragment[cursor..]);
    out
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn temp_index() -> (TextIndex, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let index = TextIndex::open_or_create(dir.path()).unwrap();
        (index, dir)
    }

    fn write(index: &TextIndex, docs: &[(&str, i64, &str)]) {
        let mut writer = index.writer().unwrap();
        for (sid, line, text) in docs {
            index
                .add_message(&writer, sid, *line, Some("user"), None, text)
                .unwrap();
        }
        writer.commit().unwrap();
    }

    #[test]
    fn search_returns_scored_hits_and_snippets() {
        let (index, _dir) = temp_index();
        write(
            &index,
            &[
                ("s1", 1, "design decision was made here"),
                ("s1", 2, "nothing relevant"),
                ("s2", 1, "the design rationale and decision matter"),
            ],
        );
        let hits = index
            .search("design decision", 10, DEFAULT_SNIPPET_CHARS)
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].score >= hits[1].score);
        assert!(hits.iter().all(|h| h.snippet.contains("«")));
    }

    #[test]
    fn and_by_default() {
        let (index, _dir) = temp_index();
        write(
            &index,
            &[
                ("s1", 1, "alpha and beta together"),
                ("s2", 1, "only alpha here"),
                ("s3", 1, "only beta here"),
            ],
        );
        let hits = index
            .search("alpha beta", 10, DEFAULT_SNIPPET_CHARS)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
    }

    #[test]
    fn invalid_query_errors() {
        let (index, _dir) = temp_index();
        write(&index, &[("s1", 1, "x")]);
        let err = index.search("\"unterminated", 10, DEFAULT_SNIPPET_CHARS);
        assert!(matches!(err, Err(IndexError::QueryParse(_))));
    }

    #[test]
    fn limit_caps_hits() {
        let (index, _dir) = temp_index();
        let docs: Vec<(&str, i64, &str)> = (0..20)
            .map(|i| ("s1", i as i64, "the term appears here"))
            .collect();
        let docs_ref: Vec<_> = docs.iter().map(|(s, l, t)| (*s, *l, *t)).collect();
        write(&index, &docs_ref);
        let hits = index.search("term", 5, DEFAULT_SNIPPET_CHARS).unwrap();
        assert_eq!(hits.len(), 5);
    }
}

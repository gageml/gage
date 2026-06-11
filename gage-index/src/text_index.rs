//! Tantivy index over `message.text`, plus the per-batch transient
//! evaluator that defines `text_search` row-wise semantics.
//!
//! The persistent index and the per-batch fallback are the same
//! engine over two index instances, so correctness reduces to both
//! being built with the same tokenizer chain. One constructor builds
//! that chain ([`register_tokenizer`]); both consumers live inside
//! this module, making the single-constructor rule structural.

use std::collections::HashSet;
use std::path::Path;

use tantivy::collector::DocSetCollector;
use tantivy::query::QueryParser;
use tantivy::schema::{
    FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
    Value as _,
};
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, IndexWriter, TantivyDocument, Term, doc};

use crate::{IndexError, Result};

/// Index format version: covers the index schema and tokenizer chain.
/// Bumping it changes the `v{N}` path component.
pub const INDEX_FORMAT_VERSION: u32 = 1;

/// Canonical identifier of the tokenizer chain, recorded in the index
/// manifest. A mismatch with running code triggers an automatic index
/// rebuild — tokenizer drift silently drops matches otherwise, since
/// the result set is `index_matches ∩ rowwise_matches`.
pub const TOKENIZER_CHAIN: &str = "simple+remove_long:40+lowercase";

const TOKENIZER_NAME: &str = "gage_default";
const MAX_TOKEN_LEN: usize = 40;

/// The single tokenizer-chain constructor: Tantivy's `default` chain
/// (split on non-alphanumeric, drop tokens over 40 bytes, lowercase).
/// Long-token removal suits this corpus — base64 blobs and hashes drop
/// out instead of bloating the index.
fn build_tokenizer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_TOKEN_LEN))
        .filter(LowerCaser)
        .build()
}

fn register_tokenizer(index: &Index) {
    index.tokenizers().register(TOKENIZER_NAME, build_tokenizer());
}

fn text_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER_NAME)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

/// The persistent index over message text. Text is not stored: the
/// index selects `(session_id, line)` coordinates; rows materialize
/// from the columnar store.
pub(crate) struct TextIndex {
    index: Index,
    f_session: Field,
    f_line: Field,
    f_text: Field,
}

fn index_schema() -> Schema {
    let mut builder = Schema::builder();
    // Raw term so `delete_term` removes a session's documents on
    // re-index.
    builder.add_text_field("session_id", STRING | STORED);
    builder.add_u64_field("line", STORED | FAST);
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
        let f_text = actual.get_field("text")?;
        Ok(Self {
            index,
            f_session,
            f_line,
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
        text: &str,
    ) -> Result<()> {
        writer.add_document(doc!(
            self.f_session => session_id,
            self.f_line => line as u64,
            self.f_text => text,
        ))?;
        Ok(())
    }

    /// Search the committed index, returning matched coordinates.
    pub(crate) fn search(&self, query: &str) -> Result<Vec<(String, i64)>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.f_text]);
        let query = parser
            .parse_query(query)
            .map_err(|e| IndexError::QueryParse(e.to_string()))?;
        let docs = searcher.search(&query, &DocSetCollector)?;
        let mut coords = Vec::with_capacity(docs.len());
        for addr in docs {
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
            coords.push((session_id, line));
        }
        Ok(coords)
    }
}

/// Row-wise `text_search` evaluation over one batch of values: build
/// a transient single-segment RAM index over the batch, run the
/// query, return the match mask (the Lucene MemoryIndex pattern).
/// This implementation defines the predicate's semantics; the
/// persistent index is purely an accelerator.
///
/// NULL inputs yield NULL (SQL predicate semantics); they are not
/// indexed, so negative queries do not match them.
pub fn text_search_mask<'a, I>(texts: I, query: &str) -> Result<Vec<Option<bool>>>
where
    I: IntoIterator<Item = Option<&'a str>>,
{
    let mut builder = Schema::builder();
    let f_row = builder.add_u64_field("row", FAST);
    let f_text = builder.add_text_field("text", text_options());
    let index = Index::create_in_ram(builder.build());
    register_tokenizer(&index);

    let mut mask: Vec<Option<bool>> = Vec::new();
    let mut writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    for (row, text) in texts.into_iter().enumerate() {
        match text {
            Some(text) => {
                mask.push(Some(false));
                writer.add_document(doc!(f_row => row as u64, f_text => text))?;
            }
            None => mask.push(None),
        }
    }
    writer.commit()?;

    let reader = index.reader()?;
    let searcher = reader.searcher();
    let parser = QueryParser::for_index(&index, vec![f_text]);
    let query = parser
        .parse_query(query)
        .map_err(|e| IndexError::QueryParse(e.to_string()))?;
    let docs: HashSet<_> = searcher.search(&query, &DocSetCollector)?;
    for addr in docs {
        let segment = searcher.segment_reader(addr.segment_ord);
        let column = segment.fast_fields().u64("row")?;
        if let Some(row) = column.first(addr.doc_id)
            && let Some(slot) = mask.get_mut(row as usize)
        {
            *slot = Some(true);
        }
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(mask: &[Option<bool>]) -> Vec<usize> {
        mask.iter()
            .enumerate()
            .filter(|(_, m)| **m == Some(true))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn mask_matches_terms() {
        let texts = vec![
            Some("a design decision was made"),
            Some("nothing relevant"),
            None,
            Some("the Design rationale"),
        ];
        let mask = text_search_mask(texts, "design").unwrap();
        assert_eq!(matched(&mask), vec![0, 3]);
        assert_eq!(mask.get(2), Some(&None));
    }

    #[test]
    fn mask_boolean_operators() {
        let texts = vec![
            Some("design and decision together"),
            Some("design only"),
            Some("decision only"),
        ];
        let mask = text_search_mask(texts.clone(), "design AND decision").unwrap();
        assert_eq!(matched(&mask), vec![0]);
        let mask = text_search_mask(texts, "design OR decision").unwrap();
        assert_eq!(matched(&mask), vec![0, 1, 2]);
    }

    #[test]
    fn mask_phrase_query() {
        let texts = vec![Some("a good reason exists"), Some("reason good a")];
        let mask = text_search_mask(texts, "\"good reason\"").unwrap();
        assert_eq!(matched(&mask), vec![0]);
    }

    #[test]
    fn mask_invalid_query_errors() {
        let texts = vec![Some("anything")];
        assert!(text_search_mask(texts, "\"unterminated").is_err());
    }

    #[test]
    fn mask_snake_case_splits() {
        let texts = vec![Some("call split_ide_tags here")];
        let mask = text_search_mask(texts, "ide").unwrap();
        assert_eq!(matched(&mask), vec![0]);
    }
}

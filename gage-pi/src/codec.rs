//! The Pi codec: a session line decoded into an entry value and
//! encoded back as one compact JSON line, with the descriptor of the
//! `pi-jsonl 1` format. One codec reads every version of Pi's format:
//! the content format names the encoding, not the generation of the
//! entries in it.

use std::sync::{Arc, LazyLock};

use gage_corpus::JsonlCodec;

use crate::text_descriptor::text_descriptor;

static CODEC: LazyLock<Arc<JsonlCodec>> =
    LazyLock::new(|| Arc::new(JsonlCodec::new(text_descriptor())));

/// The one codec of the format this build writes. The descriptor is
/// compiled once per process.
pub(crate) fn codec() -> Arc<JsonlCodec> {
    Arc::clone(&CODEC)
}

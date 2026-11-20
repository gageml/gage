//! The Pi driver's text descriptor: which fields of a session line
//! are identifiers, timestamps, signatures, or enumerations rather
//! than text, which fields each typed set holds, and which entries
//! the harness wrote for its own bookkeeping. The publish redaction
//! engine reads it.

use gage_session::{TextDescriptor, TypedSet};

/// The fields whose strings are not text
const SAFE: &[&str] = &[
    "$.type",
    "$.id",
    "$.parentId",
    "$.timestamp",
    "$.provider",
    "$.modelId",
    "$.model",
    "$.thinkingLevel",
    "$.kind",
    "$.customType",
    "$.firstKeptEntryId",
    "$.fromId",
    "$.targetId",
    "$.content[*].type",
    "$.content[*].mimeType",
    "$.content[*].data",
    "$.replacement.content[*].type",
    "$.message.role",
    "$.message.api",
    "$.message.provider",
    "$.message.model",
    "$.message.responseModel",
    "$.message.responseId",
    "$.message.stopReason",
    "$.message.rawStopReason",
    "$.message.thinkingLevel",
    "$.message.providerThinkingLevel",
    "$.message.toolCallId",
    "$.message.toolName",
    "$.message.customType",
    "$.message.deferred",
    "$.message.content[*].type",
    "$.message.content[*].id",
    "$.message.content[*].name",
    "$.message.content[*].namespace",
    "$.message.content[*].mimeType",
    "$.message.content[*].data",
    "$.systemMessage.role",
    "$..thinkingSignature",
    "$..textSignature",
    "$..thoughtSignature",
];

/// The paths of one typed set in a session line: fields whose string
/// is the sensitive literal, fields whose whole value is left out, and
/// selectors over the entry.
struct TypedLists {
    literal: &'static [&'static str],
    value: &'static [&'static str],
    entries: &'static [&'static str],
}

const NO_PATHS: &[&str] = &[];

fn typed_lists(set: TypedSet) -> TypedLists {
    match set {
        // What Pi and its extensions wrote into the model's context:
        // the system messages that carry the prompt and the tools, the
        // copy of them a compaction keeps, and the messages an
        // extension injects.
        TypedSet::HarnessContext => TypedLists {
            literal: NO_PATHS,
            value: &["$.systemMessage"],
            entries: &[
                "$[?$.type == 'message' && $.message.role == 'system']",
                "$[?$.type == 'message' && $.message.role == 'custom']",
                "$[?$.type == 'message' && $.message.role == 'hookMessage']",
                "$[?$.type == 'custom_message']",
            ],
        },
        // The harness's bookkeeping: what was selected, what a request
        // cost, an extension's state, and an edit of what a later
        // request sees. A name or a label a person gave is not
        // bookkeeping.
        TypedSet::HarnessRecord => TypedLists {
            literal: NO_PATHS,
            value: &[
                "$.version",
                "$.usage",
                "$.tokensBefore",
                "$.message.api",
                "$.message.usage",
                "$.message.responseId",
                "$.message.diagnostics",
            ],
            entries: &[
                "$[?$.type == 'model_change' || $.type == 'thinking_level_change' || $.type == 'usage' || $.type == 'custom' || $.type == 'context_edit']",
            ],
        },
        TypedSet::Pii => TypedLists {
            literal: NO_PATHS,
            value: NO_PATHS,
            entries: NO_PATHS,
        },
        TypedSet::System => TypedLists {
            literal: &["$.cwd"],
            value: &[
                "$.cwd",
                "$.parentSession",
                "$.branchedFrom",
                "$.message.fullOutputPath",
            ],
            entries: NO_PATHS,
        },
    }
}

pub(crate) fn text_descriptor() -> TextDescriptor {
    let mut descriptor = TextDescriptor::new(SAFE).expect("descriptor patterns should be JSONPath");
    for set in TypedSet::ALL {
        let paths = typed_lists(set);
        descriptor = descriptor
            .with_typed(
                set,
                paths.literal,
                NO_PATHS,
                paths.value,
                paths.entries,
                NO_PATHS,
            )
            .expect("descriptor typed patterns should be JSONPath");
    }
    descriptor
}

//! The Codex driver's text descriptor: which fields of a rollout line
//! are identifiers, timestamps, digests, or enumerations rather than
//! text, which fields each typed set holds, and which records the
//! harness wrote for its own bookkeeping. The publish redaction
//! engine reads it.

use gage_session::{TextDescriptor, TypedSet};

/// The fields whose strings are not text
const SAFE: &[&str] = &[
    "$.timestamp",
    "$.type",
    "$.payload.type",
    "$.payload.id",
    "$.payload.session_id",
    "$.payload.timestamp",
    "$.payload.forked_from_id",
    "$.payload.parent_thread_id",
    "$.payload.history_mode",
    "$.payload.thread_source",
    "$.payload.source",
    "$.payload.model_provider",
    "$.payload.context_window",
    "$.payload.call_id",
    "$.payload.role",
    "$.payload.status",
    "$.payload.phase",
    "$.payload.model",
    "$.payload.effort",
    "$.payload.response_id",
    "$.payload.item.id",
    "$.payload.item.type",
    "$.payload.item.status",
    "$.payload.item.process_id",
    "$.payload.item.source",
    "$.payload.item.phase",
    "$.payload.item.client_id",
    "$.payload.content[*].type",
    "$.payload.summary[*].type",
    "$.payload.output[*].type",
    "$.payload.item.content[*].type",
    "$.payload.internal_chat_message_metadata_passthrough",
    "$.metadata",
    "$..thread_id",
    "$..turn_id",
    "$..root_turn_id",
    "$..parent_turn_id",
    "$..encrypted_content",
    "$..image_url",
];

/// The paths of one typed set in the rollout shape: fields whose
/// string is the sensitive literal, fields whose whole value is left
/// out, and selectors over the record.
struct TypedLists {
    literal: &'static [&'static str],
    value: &'static [&'static str],
    entries: &'static [&'static str],
}

const NO_PATHS: &[&str] = &[];

fn typed_lists(set: TypedSet) -> TypedLists {
    match set {
        // What Codex wrote into the model's context: the base
        // instructions, the tool list, the developer messages, the
        // context it writes under the user role, and the state it
        // builds them from. A user-role message is the harness's when
        // its first content item is marked as the harness's; one with
        // no mark is kept.
        TypedSet::HarnessContext => TypedLists {
            literal: NO_PATHS,
            value: &["$.payload.base_instructions"],
            entries: &[
                "$[?$.type == 'world_state']",
                "$[?$.type == 'response_item' && $.payload.type == 'additional_tools']",
                "$[?$.type == 'response_item' && $.payload.type == 'message' && $.payload.role == 'developer']",
                "$[?$.type == 'response_item' && $.payload.type == 'message' && $.payload.role == 'user' && $.payload.internal_chat_message_metadata_passthrough.content_item_metadata[0].provenance.type == 'harness']",
            ],
        },
        // The harness's bookkeeping. An `item_completed` event is not
        // bookkeeping: it is the record of what a tool call ran, and
        // the `tool_use` rows are read from it.
        TypedSet::HarnessRecord => TypedLists {
            literal: NO_PATHS,
            value: &[
                "$.metadata",
                "$.payload.content[*].type",
                "$.payload.summary[*].type",
                "$.payload.output[*].type",
                "$.payload.item.content[*].type",
                "$.payload.internal_chat_message_metadata_passthrough",
                "$.payload.originator",
                "$.payload.cli_version",
                "$.payload.context_window",
            ],
            entries: &[
                "$[?$.type == 'turn_context' || $.type == 'token_usage_record' || $.type == 'retained_context' || $.type == 'dynamic_tools_updated' || $.type == 'security_risk_score']",
                "$[?$.type == 'event_msg' && $.payload.type != 'item_completed']",
            ],
        },
        TypedSet::Pii => TypedLists {
            literal: &["$.payload.creator_user_id", "$.payload.creator_account_id"],
            value: &["$.payload.creator_user_id", "$.payload.creator_account_id"],
            entries: NO_PATHS,
        },
        TypedSet::System => TypedLists {
            literal: &["$.payload.cwd"],
            value: &[
                "$.payload.cwd",
                "$.payload.runtime_workspace_roots[*]",
                "$.payload.git",
                "$.payload.thread_settings.cwd",
                "$.payload.thread_settings.runtime_workspace_roots[*]",
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

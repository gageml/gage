//! The body a viewer shows for one session entry, as the driver that
//! reads the entry's format describes it.

/// What a viewer shows for one entry: blocks in order, top to bottom.
/// The driver decides what an entry's content means to a reader; the
/// viewer decides layout, wrapping, and style.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryBody {
    pub blocks: Vec<BodyBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyBlock {
    /// Caption and value rows
    Attrs(Vec<BodyAttr>),
    /// A heading over the blocks that follow it
    Heading(String),
    /// Text the viewer renders as markdown
    Markdown(String),
    /// Text shown as written
    Text(String),
    /// A remark about the entry in place of content, such as `Empty`
    Remark(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyAttr {
    pub caption: String,
    pub value: String,
}

impl BodyAttr {
    pub fn new(caption: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            caption: caption.into(),
            value: value.into(),
        }
    }
}

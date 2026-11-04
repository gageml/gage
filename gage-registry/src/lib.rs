use std::fmt;

pub mod driver;
pub mod filter;
pub mod scanner;
pub mod scheme;

/// Where a scanner's or a filter's source file lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefSource {
    /// The extracted builtin bundle
    Builtin,
    /// The user's own root under Gage home
    User,
    /// A project root, or an ad hoc file named on the command line
    Project,
}

impl DefSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            DefSource::Builtin => "builtin",
            DefSource::User => "user",
            DefSource::Project => "project",
        }
    }
}

impl fmt::Display for DefSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

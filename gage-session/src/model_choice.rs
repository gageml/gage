//! The models a driver offers when a dialog asks which model a
//! session runs on.

/// One model a dialog offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// The model as the driver takes it
    pub model: String,
    /// The name the dialog shows
    pub label: String,
    /// A short remark shown beside the label; empty for none
    pub hint: String,
}

impl ModelChoice {
    pub fn new(model: &str, label: &str, hint: &str) -> Self {
        Self {
            model: model.to_string(),
            label: label.to_string(),
            hint: hint.to_string(),
        }
    }
}

/// What a driver offers a model dialog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelChoices {
    /// The models to pick from, best first. Empty when the driver
    /// names none, and the dialog then asks for a model name only.
    pub choices: Vec<ModelChoice>,
    /// A model name that shows the form a typed name takes
    pub example: Option<String>,
}

//! Model sizes: the names a scanner or a user gives in place of a
//! model, which each driver resolves to one of its own models.

/// The size names, smallest first.
pub const MODEL_SIZES: [&str; 3] = ["small", "medium", "large"];

/// A model for each size, where one is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSizes {
    pub small: Option<String>,
    pub medium: Option<String>,
    pub large: Option<String>,
}

impl ModelSizes {
    /// The model set for `size`. `None` when `size` is not a size name
    /// or has no model.
    pub fn get(&self, size: &str) -> Option<&str> {
        match size {
            "small" => self.small.as_deref(),
            "medium" => self.medium.as_deref(),
            "large" => self.large.as_deref(),
            _ => None,
        }
    }

    /// These models with each one `overrides` sets put in its place.
    pub fn overridden_by(&self, overrides: &ModelSizes) -> ModelSizes {
        let pick = |own: &Option<String>, over: &Option<String>| over.clone().or(own.clone());
        ModelSizes {
            small: pick(&self.small, &overrides.small),
            medium: pick(&self.medium, &overrides.medium),
            large: pick(&self.large, &overrides.large),
        }
    }

    /// Each size name with its model, smallest first.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Option<&str>)> {
        MODEL_SIZES.into_iter().map(|size| (size, self.get(size)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes(small: Option<&str>, medium: Option<&str>, large: Option<&str>) -> ModelSizes {
        ModelSizes {
            small: small.map(String::from),
            medium: medium.map(String::from),
            large: large.map(String::from),
        }
    }

    #[test]
    fn get_reads_a_size_and_nothing_else() {
        let models = sizes(Some("s"), None, Some("l"));
        assert_eq!(models.get("small"), Some("s"));
        assert_eq!(models.get("medium"), None);
        assert_eq!(models.get("large"), Some("l"));
        assert_eq!(models.get("huge"), None);
    }

    #[test]
    fn an_override_replaces_only_the_sizes_it_sets() {
        let models = sizes(Some("s"), Some("m"), Some("l"));
        let merged = models.overridden_by(&sizes(None, Some("m2"), None));
        assert_eq!(merged, sizes(Some("s"), Some("m2"), Some("l")));
        let listed: Vec<_> = merged.iter().collect();
        assert_eq!(
            listed,
            [
                ("small", Some("s")),
                ("medium", Some("m2")),
                ("large", Some("l"))
            ]
        );
    }
}

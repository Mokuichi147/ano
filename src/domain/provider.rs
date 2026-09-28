//! Which models of a provider may be used, in the same form as the tool
//! filters of MCP servers: an optional allowlist and a denylist.

/// Whether `rule` names `model`: an exact name, or a prefix ending in `*`.
pub fn model_rule_matches(rule: &str, model: &str) -> bool {
    match rule.strip_suffix('*') {
        Some(prefix) => model.starts_with(prefix),
        None => rule == model,
    }
}

/// `allowed_models` and `disabled_models` of a provider.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelFilter {
    /// When set, only matching models are enabled.
    pub allowed: Option<Vec<String>>,
    pub disabled: Vec<String>,
}

impl ModelFilter {
    pub fn is_enabled(&self, model: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|rules| rules.iter().any(|rule| model_rule_matches(rule, model)))
            && !self
                .disabled
                .iter()
                .any(|rule| model_rule_matches(rule, model))
    }

    /// Enable or disable `models` (names or `*` patterns). A filter with an
    /// allowlist keeps using it, so models the provider adds later stay
    /// disabled; otherwise disabled models are listed in `disabled`.
    ///
    /// Returns the names still in the other state because a wider pattern
    /// decides them, for example a model under `disabled = ["qwen/*"]`.
    pub fn set_enabled<'a>(
        &mut self,
        models: impl IntoIterator<Item = &'a str>,
        enabled: bool,
    ) -> Vec<&'a str> {
        let mut blocked = Vec::new();
        for model in models {
            if enabled {
                self.disabled.retain(|rule| rule != model);
                if let Some(allowed) = &mut self.allowed {
                    if !allowed.iter().any(|rule| model_rule_matches(rule, model)) {
                        allowed.push(model.to_string());
                    }
                }
            } else {
                if let Some(allowed) = &mut self.allowed {
                    allowed.retain(|rule| rule != model);
                }
                // A remaining pattern in the allowlist still enables it.
                if self.is_enabled(model) {
                    self.disabled.push(model.to_string());
                }
            }
            if self.is_enabled(model) != enabled {
                blocked.push(model);
            }
        }
        blocked
    }
}

#[cfg(test)]
mod tests {
    use super::ModelFilter;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn allowlists_and_denylists_match_exact_names_and_prefixes() {
        let filter = ModelFilter {
            allowed: Some(names(&["qwen/*", "gpt-5"])),
            disabled: names(&["qwen/qwen3-4b"]),
        };
        assert!(filter.is_enabled("qwen/qwen3-30b"));
        assert!(filter.is_enabled("gpt-5"));
        assert!(!filter.is_enabled("gpt-5-mini"));
        assert!(!filter.is_enabled("qwen/qwen3-4b"));
        assert!(ModelFilter::default().is_enabled("anything"));
    }

    #[test]
    fn toggling_keeps_the_kind_of_filter() {
        let mut open = ModelFilter::default();
        assert!(open.set_enabled(["a", "b"], false).is_empty());
        assert_eq!(open.disabled, names(&["a", "b"]));
        assert!(open.set_enabled(["a"], true).is_empty());
        assert_eq!(
            open,
            ModelFilter {
                allowed: None,
                disabled: names(&["b"])
            }
        );

        let mut listed = ModelFilter {
            allowed: Some(names(&["a", "qwen/*"])),
            disabled: Vec::new(),
        };
        assert!(listed.set_enabled(["a", "qwen/qwen3-4b"], false).is_empty());
        assert_eq!(listed.allowed, Some(names(&["qwen/*"])));
        assert_eq!(listed.disabled, names(&["qwen/qwen3-4b"]));
        assert!(listed.set_enabled(["c", "qwen/qwen3-4b"], true).is_empty());
        assert_eq!(listed.allowed, Some(names(&["qwen/*", "c"])));
        assert!(listed.disabled.is_empty());
    }

    #[test]
    fn reports_models_that_a_wider_pattern_decides() {
        let mut filter = ModelFilter {
            allowed: None,
            disabled: names(&["qwen/*"]),
        };
        assert_eq!(filter.set_enabled(["qwen/qwen3"], true), ["qwen/qwen3"]);
        assert!(!filter.is_enabled("qwen/qwen3"));
    }
}

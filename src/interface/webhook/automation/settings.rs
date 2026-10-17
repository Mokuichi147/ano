//! `[automation]` settings: the sources of events, the rules that decide
//! what is done with them, and the notifiers that tell the user. A source
//! or notifier names its `kind`, and the kind registered in the
//! `AutomationRegistry` reads and checks the rest of its settings.

use crate::{
    application::automation::{AutomationRegistry, EventSource, KindSpec, NamedNotifier},
    domain::{
        approval::ApprovalMode,
        automation::{RuleAction, TriggerRule, Trust},
        environment::EnvironmentConfig,
    },
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationSettings {
    /// Where the state of the sources is kept; `automation/state.json` in
    /// the OS data directory when unset.
    pub state_file: Option<PathBuf>,
    pub sources: BTreeMap<String, SourceSettings>,
    pub rules: Vec<TriggerRule>,
    pub notifiers: BTreeMap<String, NotifierSettings>,
}

/// One entry of `[automation.sources]`.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceSettings {
    /// The registered kind, such as `webhook`.
    pub kind: String,
    /// Whether event content is the user's own words. Each kind has its
    /// own default, and kinds whose trust is fixed refuse it.
    #[serde(default)]
    pub trust: Option<Trust>,
    /// The settings of the kind, which it checks itself.
    #[serde(flatten)]
    pub options: toml::Table,
}

/// One entry of `[automation.notifiers]`.
#[derive(Debug, Clone, Deserialize)]
pub struct NotifierSettings {
    /// The registered kind, such as `ntfy`, `discord`, `slack`, or
    /// `webhook`.
    pub kind: String,
    /// The settings of the kind, which it checks itself.
    #[serde(flatten)]
    pub options: toml::Table,
}

/// The sources and notifiers built from the settings.
pub struct AutomationParts {
    pub sources: BTreeMap<String, Arc<dyn EventSource>>,
    pub notifiers: BTreeMap<String, NamedNotifier>,
}

fn json_options(options: &toml::Table) -> Result<serde_json::Value> {
    serde_json::to_value(options).context("the settings cannot be read as JSON")
}

impl SourceSettings {
    /// Build the source `name` with its kind in `registry`.
    pub fn build(&self, name: &str, registry: &AutomationRegistry) -> Result<Arc<dyn EventSource>> {
        let options = json_options(&self.options)?;
        registry
            .build_source(
                &self.kind,
                &KindSpec {
                    name,
                    trust: self.trust,
                    options: &options,
                },
            )
            .with_context(|| format!("invalid automation.sources.{name}"))
    }
}

impl NotifierSettings {
    /// Build the notifier `name` with its kind in `registry`.
    pub fn build(&self, name: &str, registry: &AutomationRegistry) -> Result<NamedNotifier> {
        let options = json_options(&self.options)?;
        let notifier = registry
            .build_notifier(
                &self.kind,
                &KindSpec {
                    name,
                    trust: None,
                    options: &options,
                },
            )
            .with_context(|| format!("invalid automation.notifiers.{name}"))?;
        Ok(NamedNotifier {
            name: name.to_string(),
            notifier,
        })
    }
}

impl AutomationSettings {
    pub fn is_enabled(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Check what can be checked without building the sources and
    /// notifiers: names, and what the rules refer to.
    pub fn validate(
        &self,
        environments: &HashMap<String, EnvironmentConfig>,
        has_user: impl Fn(&str) -> bool,
    ) -> Result<()> {
        for (name, source) in &self.sources {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("automation source names may contain only ASCII letters, digits, '-', and '_': '{name}'");
            }
            if source.kind.trim().is_empty() {
                bail!("automation.sources.{name}.kind must not be empty");
            }
        }
        for (name, notifier) in &self.notifiers {
            if notifier.kind.trim().is_empty() {
                bail!("automation.notifiers.{name}.kind must not be empty");
            }
        }
        if !self.rules.is_empty() && self.sources.is_empty() {
            bail!("automation.rules need automation.sources to match");
        }
        for (index, rule) in self.rules.iter().enumerate() {
            let label = rule_label(index, rule);
            rule.validate()
                .with_context(|| format!("invalid {label}"))?;
            for source in &rule.sources {
                if !self.sources.contains_key(source) {
                    bail!("{label} names the unknown source '{source}'");
                }
            }
            for notifier in &rule.notify {
                if !self.notifiers.contains_key(notifier) {
                    bail!("{label} names the unknown notifier '{notifier}'");
                }
            }
            if let Some(user) = &rule.user {
                if !has_user(user) {
                    bail!("{label} runs as the unknown user '{user}'");
                }
            }
            if let Some(environment) = &rule.environment {
                if !environments.contains_key(environment) {
                    bail!("{label} runs in the unknown environment '{environment}'");
                }
            }
        }
        if let Some(path) = &self.state_file {
            if path.as_os_str().is_empty() {
                bail!("automation.state_file must not be empty");
            }
        }
        Ok(())
    }

    /// Build every source and notifier with `registry`, and check the rules
    /// against the trust of the sources they match.
    pub fn build(
        &self,
        registry: &AutomationRegistry,
        environments: &HashMap<String, EnvironmentConfig>,
    ) -> Result<AutomationParts> {
        let mut sources = BTreeMap::new();
        for (name, source) in &self.sources {
            sources.insert(name.clone(), source.build(name, registry)?);
        }
        let mut notifiers = BTreeMap::new();
        for (name, notifier) in &self.notifiers {
            notifiers.insert(name.clone(), notifier.build(name, registry)?);
        }
        for (index, rule) in self.rules.iter().enumerate() {
            let Some(environment_name) = &rule.environment else {
                continue;
            };
            let Some(environment) = environments.get(environment_name) else {
                bail!(
                    "{} runs in the unknown environment '{environment_name}'",
                    rule_label(index, rule)
                );
            };
            // Text a third party wrote must never run with every call
            // approved.
            let runs = matches!(rule.action, RuleAction::Run | RuleAction::Auto);
            let untrusted = sources
                .iter()
                .filter(|(name, _)| rule.sources.is_empty() || rule.sources.contains(name))
                .any(|(_, source)| source.trust() == Trust::Untrusted);
            if runs && untrusted && environment.effective_approval_mode() == ApprovalMode::Allow {
                bail!(
                    "{} can run tasks for untrusted events in environment '{environment_name}', whose approval_mode is \"allow\"; use \"auto\" or \"deny\" there",
                    rule_label(index, rule)
                );
            }
        }
        Ok(AutomationParts { sources, notifiers })
    }
}

fn rule_label(index: usize, rule: &TriggerRule) -> String {
    if rule.name.trim().is_empty() {
        format!("automation.rules[{index}]")
    } else {
        format!("automation rule '{}'", rule.name)
    }
}

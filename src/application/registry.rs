//! Registry of local function tools and their async handlers.

use crate::domain::{
    policy::UserPolicy,
    tool::{validate_tool_name, ToolContext, ToolDefinition},
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, RwLock},
};

#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn call(&self, arguments: Value) -> Result<Value>;
}

#[async_trait]
impl<F, Fut> ToolHandler for F
where
    F: Fn(Value) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value>> + Send,
{
    async fn call(&self, arguments: Value) -> Result<Value> {
        (self)(arguments).await
    }
}

#[async_trait]
pub trait ContextualToolHandler: Send + Sync {
    async fn call(&self, arguments: Value, context: &ToolContext) -> Result<Value>;
}

#[async_trait]
impl<F, Fut> ContextualToolHandler for F
where
    F: Fn(Value, ToolContext) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value>> + Send,
{
    async fn call(&self, arguments: Value, context: &ToolContext) -> Result<Value> {
        (self)(arguments, context.clone()).await
    }
}

enum RegisteredHandler {
    Plain(Arc<dyn ToolHandler>),
    Contextual(Arc<dyn ContextualToolHandler>),
}

struct RegisteredTool {
    definition: ToolDefinition,
    handler: RegisteredHandler,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, RegisteredTool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<F, Fut>(&self, definition: ToolDefinition, handler: F) -> Result<()>
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        self.insert(definition, RegisteredHandler::Plain(Arc::new(handler)))
    }

    pub fn register_contextual<F, Fut>(&self, definition: ToolDefinition, handler: F) -> Result<()>
    where
        F: Fn(Value, ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        self.insert(definition, RegisteredHandler::Contextual(Arc::new(handler)))
    }

    fn insert(&self, definition: ToolDefinition, handler: RegisteredHandler) -> Result<()> {
        validate_tool_name(&definition.name)?;
        let mut tools = self
            .tools
            .write()
            .map_err(|_| anyhow::anyhow!("tool registry lock poisoned"))?;

        if tools.contains_key(&definition.name) {
            bail!("tool '{}' is already registered", definition.name);
        }

        tools.insert(
            definition.name.clone(),
            RegisteredTool {
                definition,
                handler,
            },
        );
        Ok(())
    }

    pub fn definitions(&self, policy: &UserPolicy) -> Vec<ToolDefinition> {
        let Ok(tools) = self.tools.read() else {
            return Vec::new();
        };

        tools
            .values()
            .filter(|tool| {
                !policy.is_disabled(&tool.definition.name)
                    && policy.is_allowed(&tool.definition.name)
            })
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        let Ok(tools) = self.tools.read() else {
            return Vec::new();
        };
        tools.keys().cloned().collect()
    }

    pub fn definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools
            .read()
            .ok()?
            .get(name)
            .map(|tool| tool.definition.clone())
    }

    pub fn is_registered(&self, name: &str) -> bool {
        self.tools
            .read()
            .map(|tools| tools.contains_key(name))
            .unwrap_or(false)
    }

    pub async fn execute(&self, name: &str, arguments: Value) -> Result<Value> {
        self.execute_with_context(name, arguments, &ToolContext::default())
            .await
    }

    pub async fn execute_with_context(
        &self,
        name: &str,
        arguments: Value,
        context: &ToolContext,
    ) -> Result<Value> {
        let handler = {
            let tools = self
                .tools
                .read()
                .map_err(|_| anyhow::anyhow!("tool registry lock poisoned"))?;
            tools
                .get(name)
                .map(|tool| match &tool.handler {
                    RegisteredHandler::Plain(handler) => HandlerRef::Plain(Arc::clone(handler)),
                    RegisteredHandler::Contextual(handler) => {
                        HandlerRef::Contextual(Arc::clone(handler))
                    }
                })
                .with_context(|| format!("tool '{name}' is not registered"))?
        };

        match handler {
            HandlerRef::Plain(handler) => handler.call(arguments).await,
            HandlerRef::Contextual(handler) => handler.call(arguments, context).await,
        }
    }
}

enum HandlerRef {
    Plain(Arc<dyn ToolHandler>),
    Contextual(Arc<dyn ContextualToolHandler>),
}

#[cfg(test)]
mod tests {
    use super::{ToolContext, ToolDefinition, ToolRegistry};
    use crate::domain::policy::UserPolicy;
    use serde_json::json;

    #[tokio::test]
    async fn registers_filters_and_executes_tools() {
        let registry = ToolRegistry::new();
        registry
            .register(
                ToolDefinition::new("echo", "Echo input", json!({"type": "object"})),
                |arguments| async move { Ok(arguments) },
            )
            .unwrap();

        let definitions = registry.definitions(&UserPolicy::new(vec!["echo".into()], None));
        assert!(definitions.is_empty());

        let value = registry.execute("echo", json!({"ok": true})).await.unwrap();
        assert_eq!(value["ok"], true);
    }

    #[tokio::test]
    async fn contextual_handlers_receive_the_selected_environment() {
        let registry = ToolRegistry::new();
        registry
            .register_contextual(
                ToolDefinition::new(
                    "environment_name",
                    "Return the selected environment name.",
                    json!({"type": "object", "properties": {}, "additionalProperties": false}),
                ),
                |_arguments, context| async move { Ok(json!({"environment": context.environment})) },
            )
            .unwrap();

        let context = ToolContext {
            environment: "staging".into(),
            ..ToolContext::default()
        };
        let value = registry
            .execute_with_context("environment_name", json!({}), &context)
            .await
            .unwrap();
        assert_eq!(value["environment"], "staging");
    }

    #[test]
    fn rejects_names_the_api_or_policy_cannot_address() {
        let registry = ToolRegistry::new();
        for name in [
            "github:delete_issue",
            "mcp__server_0__tool_0",
            "tool_search",
            "",
        ] {
            let result = registry.register(
                ToolDefinition::new(name, "x", json!({"type": "object"})),
                |arguments| async move { Ok(arguments) },
            );
            assert!(result.is_err(), "{name} should be rejected");
        }
    }
}

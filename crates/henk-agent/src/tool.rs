//! Tools the loop can dispatch to.

use std::collections::BTreeMap;
use std::sync::Arc;

use henk_llm::ToolDef;
use serde_json::Value;

/// What a tool hands back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text for the model.
    pub content: String,
    /// Whether the call failed. The model is told either way.
    pub is_error: bool,
}

impl ToolOutput {
    /// A successful result.
    #[must_use]
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed result. The text explains the failure to the model.
    #[must_use]
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Something the model may call.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// The definition the model sees. Its name must be unique in a set.
    fn definition(&self) -> ToolDef;

    /// Runs the tool. Must not panic; failures are returned as
    /// [`ToolOutput::error`] so the model can react.
    async fn call(&self, arguments: Value) -> ToolOutput;
}

/// The tools of one agent, by model-facing name.
#[derive(Default, Clone)]
pub struct ToolSet {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.tools.keys()).finish()
    }
}

impl ToolSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool. A later tool with the same name replaces an earlier one.
    pub fn add(&mut self, tool: impl Tool + 'static) -> &mut Self {
        self.add_arc(Arc::new(tool))
    }

    /// Adds an already shared tool.
    pub fn add_arc(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.definition().name.as_str().to_owned();
        self.tools.insert(name, tool);
        self
    }

    /// The definitions, in name order.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    /// The tool behind a name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Number of tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> + '_ {
        self.tools.keys().map(String::as_str)
    }
}

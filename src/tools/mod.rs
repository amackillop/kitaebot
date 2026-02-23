//! Tool execution system.
//!
//! Tools are functions the agent can call (exec, `read_file`, `web_search`, etc.).
//! The `ToolRegistry` manages available tools and routes execution requests.
#[cfg(test)]
mod stub;

#[cfg(test)]
pub use stub::StubTool;

use crate::error::ToolError;
use crate::types::{ToolCall, ToolDefinition};
use async_trait::async_trait;
use std::collections::HashMap;

/// Tool that can be executed by the agent.
///
/// Each tool defines its name, description, parameters schema, and execution logic.
///
/// # Design Note
/// We use methods instead of associated constants for metadata (name, description)
/// because trait objects (`Box<dyn Tool>`) cannot access associated constants.
/// This design enables dynamic tool registration at the cost of small method call
/// overhead (optimized away by the compiler).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (must be unique in registry).
    fn name(&self) -> &'static str;

    /// Human-readable description of what the tool does.
    fn description(&self) -> &'static str;

    /// JSON Schema describing the tool's parameters.
    fn parameters(&self) -> serde_json::Value;

    /// Execute the tool with given arguments.
    ///
    /// # Arguments
    /// * `args` - JSON object containing the tool's arguments
    ///
    /// # Returns
    /// String result to send back to the LLM
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError>;
}

/// Registry of available tools.
///
/// Maintains a collection of tools and provides lookup and execution services.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty tool registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    ///
    /// If a tool with the same name already exists, it will be replaced.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Get tool definitions for the LLM.
    ///
    /// Returns a list of all registered tools in the format expected by the LLM API.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|t| {
                ToolDefinition::new(
                    t.name().to_string(),
                    t.description().to_string(),
                    t.parameters(),
                )
            })
            .collect()
    }

    /// Execute a tool call from the LLM.
    ///
    /// # Arguments
    /// * `call` - The tool call request from the LLM
    ///
    /// # Returns
    /// String result to send back to the LLM
    pub async fn execute(&self, call: &ToolCall) -> Result<String, ToolError> {
        let tool = self
            .tools
            .get(&call.function.name)
            .ok_or_else(|| ToolError::NotFound(call.function.name.clone()))?;

        let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        tool.execute(args).await
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_creation() {
        let registry = ToolRegistry::new();
        assert_eq!(registry.definitions().len(), 0);
    }

    #[test]
    fn test_tool_registration() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool));

        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].function.name, "stub");
    }

    #[tokio::test]
    async fn test_tool_execution() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool));

        let call = ToolCall {
            id: "test-123".to_string(),
            call_type: "function".to_string(),
            function: crate::types::ToolFunction {
                name: "stub".to_string(),
                arguments: "{}".to_string(),
            },
        };

        let result = registry.execute(&call).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Stub tool executed successfully");
    }

    #[tokio::test]
    async fn test_tool_not_found() {
        let registry = ToolRegistry::new();

        let call = ToolCall {
            id: "test-123".to_string(),
            call_type: "function".to_string(),
            function: crate::types::ToolFunction {
                name: "nonexistent".to_string(),
                arguments: "{}".to_string(),
            },
        };

        let result = registry.execute(&call).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_invalid_arguments() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StubTool));

        let call = ToolCall {
            id: "test-123".to_string(),
            call_type: "function".to_string(),
            function: crate::types::ToolFunction {
                name: "stub".to_string(),
                arguments: "invalid json".to_string(),
            },
        };

        let result = registry.execute(&call).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ToolError::InvalidArguments(_)
        ));
    }
}

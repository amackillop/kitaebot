//! Tool execution system.
//!
//! Tools are functions the agent can call (exec, `read_file`, `web_search`, etc.).

mod exec;
#[cfg(test)]
mod mock;

pub use exec::Exec;
#[cfg(test)]
pub use mock::MockTool;

use std::future::Future;
use std::pin::Pin;

use crate::error::ToolError;
use crate::types::{ToolCall, ToolDefinition};

/// A tool the agent can invoke.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> serde_json::Value;
    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>>;
}

/// Collection of available tools.
///
/// Uses `Vec` with linear scan for lookup. For small tool counts (<50),
/// this outperforms `HashMap` due to cache locality and no hashing overhead.
/// Tool execution involves HTTP calls to an LLM (100ms+), so lookup time is noise.
pub struct Tools(Vec<Box<dyn Tool>>);

impl Tools {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self(tools)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.0
            .iter()
            .map(|t| {
                ToolDefinition::new(
                    t.name().to_string(),
                    t.description().to_string(),
                    t.parameters(),
                )
            })
            .collect()
    }

    pub async fn execute(&self, call: &ToolCall) -> Result<String, ToolError> {
        let tool = self
            .0
            .iter()
            .find(|t| t.name() == call.function.name)
            .ok_or_else(|| ToolError::NotFound(call.function.name.clone()))?;

        let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        tool.execute(args).await
    }
}

impl Default for Tools {
    fn default() -> Self {
        Self::new(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolFunction;

    fn mock_call(id: &str) -> ToolCall {
        ToolCall::new(
            id.to_string(),
            ToolFunction {
                name: "mock".to_string(),
                arguments: "{}".to_string(),
            },
        )
    }

    #[test]
    fn test_definitions() {
        let tools = Tools::new(vec![Box::new(MockTool::new("ok"))]);
        let defs = tools.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mock");
    }

    #[tokio::test]
    async fn test_execute() {
        let tools = Tools::new(vec![Box::new(MockTool::new("executed"))]);
        let result = tools.execute(&mock_call("test-123")).await.unwrap();
        assert_eq!(result, "executed");
    }

    #[tokio::test]
    async fn test_not_found() {
        let tools = Tools::new(vec![]);
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "nonexistent".to_string(),
                arguments: "{}".to_string(),
            },
        );
        let result = tools.execute(&call).await;
        assert!(matches!(result.unwrap_err(), ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_invalid_arguments() {
        let tools = Tools::new(vec![Box::new(MockTool::new("ok"))]);
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "mock".to_string(),
                arguments: "invalid json".to_string(),
            },
        );
        let result = tools.execute(&call).await;
        assert!(matches!(
            result.unwrap_err(),
            ToolError::InvalidArguments(_)
        ));
    }
}

//! Tool execution system.
//!
//! Tools are functions the agent can call (exec, `read_file`, `web_search`, etc.).

mod exec;
#[cfg(test)]
mod stub;

pub use exec::Exec;
#[cfg(test)]
pub use stub::Stub;

use crate::error::ToolError;
use crate::types::{ToolCall, ToolDefinition};

/// Available tools for the agent.
pub enum Tool {
    Exec(Exec),
    #[cfg(test)]
    Stub(Stub),
}

impl Tool {
    fn name(&self) -> &'static str {
        match self {
            Self::Exec(_) => Exec::NAME,
            #[cfg(test)]
            Self::Stub(_) => Stub::NAME,
        }
    }

    fn description(&self) -> &'static str {
        match self {
            Self::Exec(_) => Exec::DESCRIPTION,
            #[cfg(test)]
            Self::Stub(_) => Stub::DESCRIPTION,
        }
    }

    fn parameters(&self) -> serde_json::Value {
        match self {
            Self::Exec(_) => Exec::parameters(),
            #[cfg(test)]
            Self::Stub(_) => Stub::parameters(),
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name().to_string(),
            self.description().to_string(),
            self.parameters(),
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        match self {
            Self::Exec(e) => e.execute(args).await,
            #[cfg(test)]
            Self::Stub(s) => s.execute(args).await,
        }
    }
}

/// Collection of available tools.
///
/// Uses `Vec` with linear scan for lookup. For small tool counts (<50),
/// this outperforms `HashMap` due to cache locality and no hashing overhead.
/// Tool execution involves HTTP calls to an LLM (100ms+), so lookup time is noise.
pub struct Tools(Vec<Tool>);

impl Tools {
    pub fn new(tools: Vec<Tool>) -> Self {
        Self(tools)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.0.iter().map(Tool::definition).collect()
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

    #[test]
    fn test_definitions() {
        let tools = Tools::new(vec![Tool::Stub(Stub)]);
        let defs = tools.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "stub");
    }

    #[tokio::test]
    async fn test_execute() {
        let tools = Tools::new(vec![Tool::Stub(Stub)]);
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "stub".to_string(),
                arguments: "{}".to_string(),
            },
        );
        let result = tools.execute(&call).await.unwrap();
        assert_eq!(result, "Stub tool executed successfully");
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
        let tools = Tools::new(vec![Tool::Stub(Stub)]);
        let call = ToolCall::new(
            "test-123".to_string(),
            ToolFunction {
                name: "stub".to_string(),
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

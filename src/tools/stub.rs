//! Stub tool for testing.

use async_trait::async_trait;

use crate::error::ToolError;
use crate::tools::Tool;

pub struct StubTool;

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn description(&self) -> &'static str {
        "A stub tool that returns a fixed response"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
        Ok("Stub tool executed successfully".to_string())
    }
}

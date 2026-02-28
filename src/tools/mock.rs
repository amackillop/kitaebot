//! Mock tool for tests.
//!
//! Returns a pre-configured output string on every call.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::ToolError;

/// Arguments for the mock tool.
#[derive(Deserialize, JsonSchema)]
struct Args {}

/// Mock tool that returns configurable output.
pub struct MockTool {
    output: String,
}

impl MockTool {
    pub const NAME: &str = "mock";
    pub const DESCRIPTION: &str = "Mock tool for testing";

    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
        }
    }

    pub fn parameters() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    #[allow(clippy::unused_async)]
    pub async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
        Ok(self.output.clone())
    }
}

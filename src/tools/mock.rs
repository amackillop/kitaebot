//! Mock tool for tests.
//!
//! Returns a pre-configured output string on every call.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::Tool;
use crate::error::ToolError;

/// Arguments for the mock tool.
#[derive(Deserialize, JsonSchema)]
struct Args {}

/// Mock tool that returns configurable output.
pub struct MockTool {
    output: String,
}

impl MockTool {
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
        }
    }
}

impl Tool for MockTool {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn description(&self) -> &'static str {
        "Mock tool for testing"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        _args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let output = self.output.clone();
        Box::pin(async move { Ok(output) })
    }
}

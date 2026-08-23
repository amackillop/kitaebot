//! Mock tools for tests.

use std::future::Future;
use std::pin::Pin;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{Tool, ToolCtx};
use crate::error::ToolError;

/// Arguments for mock tools (accepts anything).
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
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let output = self.output.clone();
        Box::pin(async move { Ok(output) })
    }
}

/// Mock tool that always returns `ToolError::Blocked`.
pub struct MockBlockedTool {
    name: &'static str,
    guidance: String,
}

impl MockBlockedTool {
    pub fn new(guidance: impl Into<String>) -> Self {
        Self::named("mock_blocked", guidance)
    }

    /// A named instance, so tests can register several rules at once.
    pub fn named(name: &'static str, guidance: impl Into<String>) -> Self {
        Self {
            name,
            guidance: guidance.into(),
        }
    }
}

impl Tool for MockBlockedTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "Mock tool that always blocks"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let name = self.name;
        let guidance = self.guidance.clone();
        Box::pin(async move {
            Err(ToolError::Blocked {
                operation: name.into(),
                guidance,
            })
        })
    }
}

/// Mock tool that always returns a configurable `ToolError`. Takes a
/// closure that produces a fresh error each call, since `ToolError`
/// is not `Clone`. For testing the tool-strike escalation path
/// (issue #45).
pub struct MockFailingTool {
    name: &'static str,
    make_error: Box<dyn Fn() -> ToolError + Send + Sync>,
}

impl MockFailingTool {
    pub fn named(
        name: &'static str,
        make_error: impl Fn() -> ToolError + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            make_error: Box::new(make_error),
        }
    }
}

impl Tool for MockFailingTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "Mock tool that always fails"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        let error = (self.make_error)();
        Box::pin(async move { Err(error) })
    }
}

//! Stub tool for testing.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::ToolError;

/// Arguments for the stub tool.
#[derive(Deserialize, JsonSchema)]
struct Args {}

/// A no-op tool that returns a fixed response.
pub struct Stub;

impl Stub {
    pub const NAME: &str = "stub";
    pub const DESCRIPTION: &str = "A stub tool for testing";

    pub fn parameters() -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    #[allow(clippy::unused_async)] // Async for interface consistency
    pub async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
        Ok("Stub tool executed successfully".to_string())
    }
}

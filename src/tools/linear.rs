//! Linear issue tools.
//!
//! Lets the agent move a ticket to a named workflow state. The state
//! name is resolved against the issue's team, so workflows without a
//! given state report the available ones instead of failing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use super::{Tool, ToolCtx};
use crate::clients::linear::{LinearClient, SetStateOutcome};
use crate::error::ToolError;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// Issue identifier, e.g. `"MDK-123"`.
    issue: String,
    /// Target workflow state name, e.g. `"In Progress"`
    /// (case-insensitive).
    state: String,
}

pub struct LinearSetState(pub LinearClient);

impl Tool for LinearSetState {
    fn name(&self) -> &'static str {
        "linear_set_state"
    }

    fn description(&self) -> &'static str {
        "Move a Linear issue to a named workflow state. If the workflow \
         has no state by that name, the reply lists the available states \
         so you can pick a valid one or leave the state unchanged."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(Args)).expect("schema serialization failed")
    }

    fn execute(
        &self,
        args: serde_json::Value,
        _ctx: ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + '_>> {
        Box::pin(async move {
            let args: Args = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
            let outcome = self.0.set_state(&args.issue, &args.state).await?;
            Ok(format_outcome(&args.issue, &args.state, &outcome))
        })
    }
}

/// Pure: render a [`SetStateOutcome`] as the tool's reply.
fn format_outcome(issue: &str, requested: &str, outcome: &SetStateOutcome) -> String {
    match outcome {
        SetStateOutcome::Moved { state } => format!("Moved {issue} to \"{state}\"."),
        SetStateOutcome::NoSuchState { available } => format!(
            "This workflow has no state named \"{requested}\". Available states: {}. \
             Leave the state unchanged if none fit.",
            available.join(", ")
        ),
    }
}

/// Build the Linear tools from a pre-constructed [`LinearClient`].
pub(crate) fn build(client: LinearClient) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(LinearSetState(client))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_moved() {
        let outcome = SetStateOutcome::Moved {
            state: "In Progress".into(),
        };
        assert_eq!(
            format_outcome("MDK-1", "in progress", &outcome),
            "Moved MDK-1 to \"In Progress\"."
        );
    }

    #[test]
    fn format_no_such_state_lists_available() {
        let outcome = SetStateOutcome::NoSuchState {
            available: vec!["Todo".into(), "Done".into()],
        };
        let msg = format_outcome("MDK-1", "Plan Review", &outcome);
        assert!(msg.contains("no state named \"Plan Review\""));
        assert!(msg.contains("Todo, Done"));
    }

    #[tokio::test]
    async fn execute_moves_issue() {
        use crate::clients::RawResponse;

        let client = LinearClient::from_fn(|body| async move {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let payload = if req["query"].as_str().unwrap().contains("issueUpdate") {
                r#"{"data":{"issueUpdate":{"success":true}}}"#
            } else {
                r#"{"data":{"issue":{"id":"uuid-1","team":{"states":{"nodes":[
                    {"id":"s-prog","name":"In Progress"}
                ]}}}}}"#
            };
            Ok(RawResponse {
                status: 200,
                body: payload.as_bytes().to_vec(),
            })
        });
        let tool = LinearSetState(client);

        let out = tool
            .execute(
                serde_json::json!({ "issue": "MDK-1", "state": "In Progress" }),
                ToolCtx::default(),
            )
            .await
            .unwrap();

        assert_eq!(out, "Moved MDK-1 to \"In Progress\".");
    }
}

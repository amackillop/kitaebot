//! Tools that require network access (HTTP clients).

mod web_fetch;
mod web_search;

pub use web_fetch::WebFetch;
pub use web_search::WebSearch;

// Re-export parent utilities so child modules can use `super::`.
pub(crate) use super::{Tool, truncate_output};

use tracing::error;

use crate::clients::chat_completion::CompletionsClient;
use crate::config::Config;

/// Build the web tools.
pub fn build(config: &Config, client: CompletionsClient) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(WebFetch::new(&config.tools.web_fetch).unwrap_or_else(|e| {
            error!("Failed to initialize web_fetch: {e}");
            std::process::exit(1);
        })),
        Box::new(WebSearch::new(client, &config.tools.web_search)),
    ]
}

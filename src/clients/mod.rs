pub mod chat_completion;
pub mod telegram;

/// Raw HTTP response at the IO/pure boundary.
///
/// The effectful layer (closure) produces this; the pure layer
/// (`interpret_response`) consumes it. No `reqwest` types leak across.
pub struct RawResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

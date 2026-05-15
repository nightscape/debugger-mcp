//! Wire-level encoding of tool results.
//!
//! Every tool handler returns a `serde_json::Value`; this module is the single
//! place that turns it into the text placed in the MCP `content` block sent to
//! the AI agent. Two encodings are supported:
//!
//! - [`OutputFormat::Json`] — pretty-printed JSON (the historical behaviour).
//! - [`OutputFormat::Toon`] — Token-Oriented Object Notation, a lossless but far
//!   more token-efficient rendering of the same JSON data model
//!   (<https://github.com/toon-format/toon>).
//!
//! The format is selected once, at process start, via the
//! `DEBUGGER_MCP_OUTPUT_FORMAT` environment variable (`json` | `toon`).

use serde_json::Value;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Json,
    Toon,
}

impl OutputFormat {
    /// The format configured for this process, read once from
    /// `DEBUGGER_MCP_OUTPUT_FORMAT`. Defaults to [`OutputFormat::Toon`] for its
    /// ~45% token savings; set the variable to `json` to opt back into
    /// pretty-printed JSON (e.g. for debugging the wire output). An
    /// unrecognised value is a startup misconfiguration and panics loudly
    /// rather than silently degrading.
    pub fn from_env() -> Self {
        static CACHED: OnceLock<OutputFormat> = OnceLock::new();
        *CACHED.get_or_init(|| match std::env::var("DEBUGGER_MCP_OUTPUT_FORMAT") {
            Err(_) => OutputFormat::Toon,
            Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
                "json" => OutputFormat::Json,
                "toon" => OutputFormat::Toon,
                other => panic!(
                    "invalid DEBUGGER_MCP_OUTPUT_FORMAT={other:?}, expected \"json\" or \"toon\""
                ),
            },
        })
    }

    /// Render a tool-result value as the text that goes on the wire.
    ///
    /// Both encoders operate on the JSON data model and cover every
    /// `serde_json::Value`, so a failure here indicates a bug in the encoder,
    /// not bad input — we surface it as a panic rather than hiding it behind a
    /// placeholder string.
    pub fn encode(self, value: &Value) -> String {
        match self {
            OutputFormat::Json => serde_json::to_string_pretty(value)
                .expect("serde_json cannot fail to serialize a Value"),
            OutputFormat::Toon => toon_format::encode_default(value)
                .expect("toon-format cannot fail to encode a JSON Value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trips_through_the_encoder() {
        let v = json!({"state": "Stopped", "line": 42});
        let out = OutputFormat::Json.encode(&v);
        let back: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn toon_encodes_uniform_arrays_tabularly() {
        let v = json!({
            "frames": [
                {"id": 1, "name": "main"},
                {"id": 2, "name": "inner"}
            ]
        });
        let out = OutputFormat::Toon.encode(&v);
        // TOON collapses a uniform object array into a header + rows.
        assert!(out.contains("frames[2]{id,name}:"), "got:\n{out}");
        assert!(out.contains("1,main"), "got:\n{out}");
    }
}

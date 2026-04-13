use serde::{Deserialize, Serialize};
use serde_json::Value;

/// DAP Protocol Message
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "request")]
    Request(Request),
    #[serde(rename = "response")]
    Response(Response),
    #[serde(rename = "event")]
    Event(Event),
}

/// DAP Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub seq: i32,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

/// DAP Response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub seq: i32,
    pub request_seq: i32,
    pub command: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

/// DAP Event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: i32,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

/// Initialize Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequestArguments {
    #[serde(rename = "clientID")]
    pub client_id: Option<String>,
    pub client_name: Option<String>,
    #[serde(rename = "adapterID")]
    pub adapter_id: String,
    pub locale: Option<String>,
    pub lines_start_at_1: Option<bool>,
    pub columns_start_at_1: Option<bool>,
    pub path_format: Option<String>,
    /// Tell the adapter we support variable paging so it honors
    /// `start`/`count` in variables requests.
    pub supports_variable_paging: Option<bool>,
}

/// Capabilities returned by initialize
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub supports_configuration_done_request: Option<bool>,
    pub supports_function_breakpoints: Option<bool>,
    pub supports_conditional_breakpoints: Option<bool>,
    pub supports_hit_conditional_breakpoints: Option<bool>,
    pub supports_evaluate_for_hovers: Option<bool>,
    pub supports_set_variable: Option<bool>,
    pub supports_restart_frame: Option<bool>,
    pub supports_step_in_targets_request: Option<bool>,
    pub supports_cancel_request: Option<bool>,
    pub supports_data_breakpoints: Option<bool>,
}

/// Launch Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequestArguments {
    pub no_debug: Option<bool>,
    #[serde(flatten)]
    pub additional: Value,
}

/// SetBreakpoints Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetBreakpointsArguments {
    pub source: Source,
    pub breakpoints: Option<Vec<SourceBreakpoint>>,
    pub source_modified: Option<bool>,
}

/// Source reference
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub name: Option<String>,
    pub path: Option<String>,
    pub source_reference: Option<i32>,
}

/// Source breakpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceBreakpoint {
    pub line: i32,
    pub column: Option<i32>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

/// Breakpoint response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
    pub id: Option<i32>,
    pub verified: bool,
    pub message: Option<String>,
    pub source: Option<Source>,
    pub line: Option<i32>,
    pub column: Option<i32>,
}

/// StackTrace Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackTraceArguments {
    pub thread_id: i32,
    pub start_frame: Option<i32>,
    pub levels: Option<i32>,
}

/// Stack Frame
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackFrame {
    pub id: i32,
    pub name: String,
    pub source: Option<Source>,
    pub line: i32,
    pub column: i32,
    pub end_line: Option<i32>,
    pub end_column: Option<i32>,
}

/// Thread info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: i32,
    pub name: String,
}

/// CodeLLDB-extended ValueFormat. The DAP spec defines `hex: bool`; CodeLLDB
/// adds `showRaw` which when true skips synthetic children providers and
/// exposes the underlying struct fields. For large containers (Vec, HashMap,
/// BTreeMap, BTreeSet) this is dramatically cheaper because the synthetic
/// provider doesn't have to materialise per-element children before answering.
///
/// Both fields are optional; serializing an all-None ValueFormat is harmless
/// — DAP-spec adapters will just ignore unrecognised fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueFormat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hex: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_raw: Option<bool>,
}

/// Evaluate Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateArguments {
    pub expression: String,
    pub frame_id: Option<i32>,
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ValueFormat>,
}

/// Variable
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variable {
    pub name: String,
    pub value: String,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub variables_reference: i32,
}

/// Scopes Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopesArguments {
    pub frame_id: i32,
}

/// Scope
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    pub name: String,
    pub variables_reference: i32,
    pub expensive: bool,
}

/// Variables Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VariablesArguments {
    pub variables_reference: i32,
    pub filter: Option<String>,
    pub start: Option<i32>,
    pub count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ValueFormat>,
}

/// Continue Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueArguments {
    pub thread_id: i32,
}

/// Next (Step Over) Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextArguments {
    pub thread_id: i32,
}

/// StepIn (Step Into) Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepInArguments {
    pub thread_id: i32,
}

/// SetExceptionBreakpoints Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetExceptionBreakpointsArguments {
    pub filters: Vec<String>,
}

/// StepOut (Step Out) Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepOutArguments {
    pub thread_id: i32,
}

/// DataBreakpointInfo Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataBreakpointInfoArguments {
    /// Variable name or expression to get data breakpoint info for.
    pub name: String,
    /// Variables reference (from a scope or parent variable). Required when
    /// `name` refers to a child variable rather than a top-level expression.
    pub variables_reference: Option<i32>,
    pub frame_id: Option<i32>,
}

/// DataBreakpointInfo Response body
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataBreakpointInfoBody {
    /// Opaque identifier for the data. null if not available.
    pub data_id: Option<String>,
    /// Human-readable description of the data.
    pub description: String,
    /// Attribute lists the available access types for this data. If empty,
    /// the data breakpoint cannot be set.
    #[serde(default)]
    pub access_types: Vec<String>,
    pub can_persist: Option<bool>,
}

/// A single data breakpoint to set
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataBreakpoint {
    pub data_id: String,
    /// Access type: "read", "write", or "readWrite"
    pub access_type: Option<String>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
}

/// SetDataBreakpoints Request Arguments
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDataBreakpointsArguments {
    pub breakpoints: Vec<DataBreakpoint>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_request_serialization() {
        let req = Request {
            seq: 1,
            command: "initialize".to_string(),
            arguments: Some(json!({"clientID": "test"})),
        };

        let serialized = serde_json::to_string(&req).unwrap();
        assert!(serialized.contains("initialize"));
        assert!(serialized.contains("\"seq\":1"));
    }

    #[test]
    fn test_response_serialization() {
        let resp = Response {
            seq: 2,
            request_seq: 1,
            command: "initialize".to_string(),
            success: true,
            message: None,
            body: Some(json!({"capabilities": {}})),
        };

        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(serialized.contains("\"success\":true"));
    }

    #[test]
    fn test_source_breakpoint() {
        let bp = SourceBreakpoint {
            line: 10,
            column: Some(5),
            condition: Some("x > 0".to_string()),
            hit_condition: None,
        };

        assert_eq!(bp.line, 10);
        assert_eq!(bp.column, Some(5));
    }

    #[test]
    fn test_stack_frame() {
        let frame = StackFrame {
            id: 1,
            name: "main".to_string(),
            source: Some(Source {
                name: Some("test.py".to_string()),
                path: Some("/path/to/test.py".to_string()),
                source_reference: None,
            }),
            line: 42,
            column: 10,
            end_line: None,
            end_column: None,
        };

        assert_eq!(frame.name, "main");
        assert_eq!(frame.line, 42);
    }
}

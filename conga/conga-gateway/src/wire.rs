//! Inbound wire protocol types sent by the frontend over WebSocket/JSON.
//! (The outbound event schema is owned by the host: `conga_host::wire`.)

use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct IncomingMessage {
    #[serde(rename = "type")]
    pub(crate) msg_type: String,
    pub(crate) content: Option<String>,
    pub(crate) trace_id: Option<String>,
}

/// Inbound `{"type":"approval_response","request_id":"ap1","approved":true,"remember":false}`
/// from the frontend. `remember` is optional (defaults false).
#[derive(Deserialize)]
pub(crate) struct ApprovalResponse {
    pub(crate) request_id: String,
    pub(crate) approved: bool,
    #[serde(default)]
    pub(crate) remember: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_message_parses_type_content_and_trace_id() {
        let m: IncomingMessage =
            serde_json::from_str(r#"{"type":"message","content":"hi","trace_id":"t1"}"#).unwrap();
        assert_eq!(m.msg_type, "message");
        assert_eq!(m.content.as_deref(), Some("hi"));
        assert_eq!(m.trace_id.as_deref(), Some("t1"));
    }

    #[test]
    fn incoming_message_allows_missing_optional_fields() {
        let m: IncomingMessage = serde_json::from_str(r#"{"type":"cancel"}"#).unwrap();
        assert_eq!(m.msg_type, "cancel");
        assert!(m.content.is_none());
        assert!(m.trace_id.is_none());
    }

    #[test]
    fn incoming_message_requires_the_type_field() {
        assert!(serde_json::from_str::<IncomingMessage>(r#"{"content":"x"}"#).is_err());
        assert!(serde_json::from_str::<IncomingMessage>(r#"{}"#).is_err());
    }

    #[test]
    fn approval_response_defaults_remember_to_false() {
        let a: ApprovalResponse =
            serde_json::from_str(r#"{"request_id":"ap1","approved":true}"#).unwrap();
        assert_eq!(a.request_id, "ap1");
        assert!(a.approved);
        assert!(!a.remember, "remember is optional and defaults to false");
    }

    #[test]
    fn approval_response_reads_the_remember_flag() {
        let a: ApprovalResponse =
            serde_json::from_str(r#"{"request_id":"ap1","approved":false,"remember":true}"#)
                .unwrap();
        assert_eq!(a.request_id, "ap1");
        assert!(!a.approved);
        assert!(a.remember);
    }

    #[test]
    fn approval_response_requires_the_request_id_and_the_decision() {
        assert!(serde_json::from_str::<ApprovalResponse>(r#"{"approved":true}"#).is_err());
        assert!(
            serde_json::from_str::<ApprovalResponse>(r#"{"request_id":"ap1"}"#).is_err(),
            "the decision is mandatory"
        );
    }
}

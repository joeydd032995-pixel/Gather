//! Row/JSON ↔ protobuf conversions shared by the gRPC service impls.

use chrono::{DateTime, Utc};
use prost_types::{value::Kind, ListValue, Struct, Timestamp, Value as ProstValue};
use serde_json::Value as JsonValue;

use super::pb;

pub fn timestamp(dt: Option<DateTime<Utc>>) -> Option<Timestamp> {
    dt.map(|dt| Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    })
}

pub fn prost_struct(json: &JsonValue) -> Option<Struct> {
    match prost_value(json).kind {
        Some(Kind::StructValue(s)) => Some(s),
        _ => None,
    }
}

fn prost_value(json: &JsonValue) -> ProstValue {
    let kind = match json {
        JsonValue::Null => Kind::NullValue(0),
        JsonValue::Bool(b) => Kind::BoolValue(*b),
        JsonValue::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        JsonValue::String(s) => Kind::StringValue(s.clone()),
        JsonValue::Array(items) => Kind::ListValue(ListValue {
            values: items.iter().map(prost_value).collect(),
        }),
        JsonValue::Object(map) => Kind::StructValue(Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), prost_value(v)))
                .collect(),
        }),
    };
    ProstValue { kind: Some(kind) }
}

pub fn artifact_kind_to_pb(kind: &str) -> pb::ArtifactKind {
    match kind {
        "chat_export" => pb::ArtifactKind::ChatExport,
        "agent_log" => pb::ArtifactKind::AgentLog,
        "document_pdf" => pb::ArtifactKind::DocumentPdf,
        "document_markdown" => pb::ArtifactKind::DocumentMarkdown,
        "document_text" => pb::ArtifactKind::DocumentText,
        "image_photo" => pb::ArtifactKind::ImagePhoto,
        "image_screenshot" => pb::ArtifactKind::ImageScreenshot,
        _ => pb::ArtifactKind::Unspecified,
    }
}

pub fn artifact_kind_from_pb(kind: pb::ArtifactKind) -> Option<&'static str> {
    match kind {
        pb::ArtifactKind::ChatExport => Some("chat_export"),
        pb::ArtifactKind::AgentLog => Some("agent_log"),
        pb::ArtifactKind::DocumentPdf => Some("document_pdf"),
        pb::ArtifactKind::DocumentMarkdown => Some("document_markdown"),
        pb::ArtifactKind::DocumentText => Some("document_text"),
        pb::ArtifactKind::ImagePhoto => Some("image_photo"),
        pb::ArtifactKind::ImageScreenshot => Some("image_screenshot"),
        pb::ArtifactKind::Unspecified => None,
    }
}

pub fn unit_kind_to_pb(kind: &str) -> pb::UnitKind {
    match kind {
        "fact" => pb::UnitKind::Fact,
        "claim" => pb::UnitKind::Claim,
        "decision" => pb::UnitKind::Decision,
        "preference" => pb::UnitKind::Preference,
        "event" => pb::UnitKind::Event,
        _ => pb::UnitKind::Unspecified,
    }
}

pub fn unit_kind_from_pb(kind: pb::UnitKind) -> Option<&'static str> {
    match kind {
        pb::UnitKind::Fact => Some("fact"),
        pb::UnitKind::Claim => Some("claim"),
        pb::UnitKind::Decision => Some("decision"),
        pb::UnitKind::Preference => Some("preference"),
        pb::UnitKind::Event => Some("event"),
        pb::UnitKind::Unspecified => None,
    }
}

pub fn unit_status_to_pb(status: &str) -> pb::UnitStatus {
    match status {
        "active" => pb::UnitStatus::Active,
        "superseded" => pb::UnitStatus::Superseded,
        "retracted" => pb::UnitStatus::Retracted,
        "disputed" => pb::UnitStatus::Disputed,
        _ => pb::UnitStatus::Unspecified,
    }
}

pub fn unit_status_from_pb(status: pb::UnitStatus) -> Option<&'static str> {
    match status {
        pb::UnitStatus::Active => Some("active"),
        pb::UnitStatus::Superseded => Some("superseded"),
        pb::UnitStatus::Retracted => Some("retracted"),
        pb::UnitStatus::Disputed => Some("disputed"),
        pb::UnitStatus::Unspecified => None,
    }
}

pub fn contradiction_status_to_pb(status: &str) -> pb::ContradictionStatus {
    match status {
        "open" => pb::ContradictionStatus::Open,
        "resolved_a" => pb::ContradictionStatus::ResolvedA,
        "resolved_b" => pb::ContradictionStatus::ResolvedB,
        "both_valid" => pb::ContradictionStatus::BothValid,
        "dismissed" => pb::ContradictionStatus::Dismissed,
        _ => pb::ContradictionStatus::Unspecified,
    }
}

pub fn contradiction_status_from_pb(status: pb::ContradictionStatus) -> Option<&'static str> {
    match status {
        pb::ContradictionStatus::Open => Some("open"),
        pb::ContradictionStatus::ResolvedA => Some("resolved_a"),
        pb::ContradictionStatus::ResolvedB => Some("resolved_b"),
        pb::ContradictionStatus::BothValid => Some("both_valid"),
        pb::ContradictionStatus::Dismissed => Some("dismissed"),
        pb::ContradictionStatus::Unspecified => None,
    }
}

/// Empty proto strings mean "unset" (proto3 has no optional strings here).
pub fn opt(s: Option<String>) -> String {
    s.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_round_trips_into_prost_struct() {
        let v = json!({"a": 1, "b": ["x", true], "c": {"nested": null}});
        let s = prost_struct(&v).unwrap();
        assert_eq!(s.fields.len(), 3);
        assert!(matches!(
            s.fields["a"].kind,
            Some(Kind::NumberValue(n)) if (n - 1.0).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn enum_maps_are_inverse() {
        for kind in [
            "chat_export",
            "agent_log",
            "document_pdf",
            "document_markdown",
            "document_text",
            "image_photo",
            "image_screenshot",
        ] {
            assert_eq!(artifact_kind_from_pb(artifact_kind_to_pb(kind)), Some(kind));
        }
        for status in [
            "open",
            "resolved_a",
            "resolved_b",
            "both_valid",
            "dismissed",
        ] {
            assert_eq!(
                contradiction_status_from_pb(contradiction_status_to_pb(status)),
                Some(status)
            );
        }
    }
}

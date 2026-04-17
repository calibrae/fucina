#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// --- Enums ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TaskResult {
    #[default]
    #[serde(rename = "RESULT_UNSPECIFIED", alias = "0")]
    Unspecified = 0,
    #[serde(rename = "RESULT_SUCCESS", alias = "1")]
    Success = 1,
    #[serde(rename = "RESULT_FAILURE", alias = "2")]
    Failure = 2,
    #[serde(rename = "RESULT_CANCELLED", alias = "3")]
    Cancelled = 3,
    #[serde(rename = "RESULT_SKIPPED", alias = "4")]
    Skipped = 4,
}

// --- Timestamps ---
// Protobuf JSON encodes google.protobuf.Timestamp as RFC 3339 string

#[derive(Debug, Clone)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanos: i32,
}

impl Timestamp {
    pub fn now() -> Self {
        let now = chrono::Utc::now();
        Self {
            seconds: now.timestamp(),
            nanos: now.timestamp_subsec_nanos() as i32,
        }
    }

    fn to_rfc3339(&self) -> String {
        chrono::DateTime::from_timestamp(self.seconds, self.nanos.max(0) as u32)
            .unwrap_or_default()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de;

        struct TimestampVisitor;
        impl<'de> de::Visitor<'de> for TimestampVisitor {
            type Value = Timestamp;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an RFC 3339 timestamp string or {seconds, nanos} object")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Timestamp, E> {
                let dt = chrono::DateTime::parse_from_rfc3339(v).map_err(de::Error::custom)?;
                Ok(Timestamp {
                    seconds: dt.timestamp(),
                    nanos: dt.timestamp_subsec_nanos() as i32,
                })
            }
            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Timestamp, A::Error> {
                let mut seconds = 0i64;
                let mut nanos = 0i32;
                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "seconds" => seconds = map.next_value()?,
                        "nanos" => nanos = map.next_value()?,
                        _ => {
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(Timestamp { seconds, nanos })
            }
        }
        deserializer.deserialize_any(TimestampVisitor)
    }
}

// --- Runner ---

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Runner {
    #[serde(deserialize_with = "deserialize_i64")]
    pub id: i64,
    pub uuid: String,
    pub token: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub ephemeral: bool,
}

// --- Register ---

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterRequest {
    pub name: String,
    pub token: String,
    pub version: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RegisterResponse {
    pub runner: Option<Runner>,
}

// --- Declare ---

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclareRequest {
    pub version: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct DeclareResponse {
    pub runner: Option<Runner>,
}

// --- FetchTask ---

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchTaskRequest {
    #[serde(serialize_with = "serialize_i64")]
    pub tasks_version: i64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FetchTaskResponse {
    pub task: Option<Task>,
    #[serde(deserialize_with = "deserialize_i64")]
    pub tasks_version: i64,
}

// --- Task ---

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Task {
    #[serde(deserialize_with = "deserialize_i64")]
    pub id: i64,
    /// Base64-encoded workflow YAML
    pub workflow_payload: String,
    /// GitHub context as JSON object
    pub context: serde_json::Value,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    #[serde(default)]
    pub needs: HashMap<String, TaskNeed>,
    #[serde(default)]
    pub vars: HashMap<String, String>,
    #[serde(default)]
    pub machine: String,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            id: 0,
            workflow_payload: String::new(),
            context: serde_json::Value::Null,
            secrets: HashMap::new(),
            needs: HashMap::new(),
            vars: HashMap::new(),
            machine: String::new(),
        }
    }
}

// --- TaskNeed ---

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskNeed {
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    pub result: TaskResult,
}

// --- TaskState ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskState {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub result: TaskResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepState>,
}

// --- StepState ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepState {
    #[serde(serialize_with = "serialize_i64")]
    pub id: i64,
    pub result: TaskResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<Timestamp>,
    #[serde(serialize_with = "serialize_i64")]
    pub log_index: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub log_length: i64,
}

// --- UpdateTask ---

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTaskRequest {
    pub state: TaskState,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub outputs: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateTaskResponse {
    pub state: Option<serde_json::Value>,
    #[serde(default)]
    pub sent_outputs: Vec<String>,
}

// --- UpdateLog ---

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateLogRequest {
    #[serde(serialize_with = "serialize_i64")]
    pub task_id: i64,
    #[serde(serialize_with = "serialize_i64")]
    pub index: i64,
    pub rows: Vec<LogRow>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_more: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateLogResponse {
    #[serde(deserialize_with = "deserialize_i64")]
    pub ack_index: i64,
}

// --- LogRow ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRow {
    pub time: Timestamp,
    pub content: String,
}

// --- Serde helpers for i64 (protobuf JSON encodes int64 as string) ---

fn deserialize_i64<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct I64Visitor;
    impl<'de> de::Visitor<'de> for I64Visitor {
        type Value = i64;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an integer or string-encoded integer")
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<i64, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<i64, E> {
            if v.is_empty() {
                return Ok(0);
            }
            v.parse().map_err(de::Error::custom)
        }
    }
    deserializer.deserialize_any(I64Visitor)
}

fn serialize_i64<S>(val: &i64, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&val.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_task_result_from_string() {
        let r: TaskResult = serde_json::from_str(r#""RESULT_SUCCESS""#).unwrap();
        assert_eq!(r, TaskResult::Success);
    }

    #[test]
    fn deserialize_task_result_from_number_alias() {
        // protobuf JSON may send integer as string alias
        let r: TaskResult = serde_json::from_str(r#""0""#).unwrap();
        assert_eq!(r, TaskResult::Unspecified);
    }

    #[test]
    fn serialize_task_result() {
        let json = serde_json::to_string(&TaskResult::Failure).unwrap();
        assert_eq!(json, r#""RESULT_FAILURE""#);
    }

    #[test]
    fn deserialize_runner_with_string_id() {
        let json =
            r#"{"id":"42","uuid":"abc","token":"tok","name":"r1","version":"1.0","labels":["x"]}"#;
        let runner: Runner = serde_json::from_str(json).unwrap();
        assert_eq!(runner.id, 42);
        assert_eq!(runner.uuid, "abc");
        assert_eq!(runner.labels, vec!["x"]);
    }

    #[test]
    fn deserialize_runner_with_numeric_id() {
        let json = r#"{"id":7,"uuid":"u","token":"t","name":"n"}"#;
        let runner: Runner = serde_json::from_str(json).unwrap();
        assert_eq!(runner.id, 7);
    }

    #[test]
    fn deserialize_runner_empty_id() {
        let json = r#"{"id":"","uuid":"u","token":"t","name":"n"}"#;
        let runner: Runner = serde_json::from_str(json).unwrap();
        assert_eq!(runner.id, 0);
    }

    #[test]
    fn deserialize_runner_defaults() {
        let json = r#"{}"#;
        let runner: Runner = serde_json::from_str(json).unwrap();
        assert_eq!(runner.id, 0);
        assert!(runner.labels.is_empty());
        assert!(!runner.ephemeral);
    }

    #[test]
    fn serialize_task_state_camel_case() {
        let state = TaskState {
            id: 123,
            result: TaskResult::Success,
            started_at: Some(Timestamp {
                seconds: 1000,
                nanos: 0,
            }),
            stopped_at: None,
            steps: vec![],
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains(r#""id":"123""#));
        assert!(json.contains(r#""result":"RESULT_SUCCESS""#));
        // startedAt should be an RFC 3339 string, not an object
        assert!(json.contains(r#""startedAt":"1970-"#), "got: {}", json);
        assert!(
            !json.contains(r#""seconds""#),
            "timestamp should not be object: {}",
            json
        );
        // stopped_at is None — should be omitted
        assert!(!json.contains("stoppedAt"));
        // steps is empty — should be omitted
        assert!(!json.contains("steps"));
    }

    #[test]
    fn serialize_step_state_i64_as_strings() {
        let step = StepState {
            id: 5,
            result: TaskResult::Failure,
            started_at: None,
            stopped_at: None,
            log_index: 100,
            log_length: 42,
        };
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains(r#""id":"5""#));
        assert!(json.contains(r#""logIndex":"100""#));
        assert!(json.contains(r#""logLength":"42""#));
    }

    #[test]
    fn serialize_update_log_request_omits_no_more_false() {
        let req = UpdateLogRequest {
            task_id: 1,
            index: 0,
            rows: vec![],
            no_more: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("noMore"));
    }

    #[test]
    fn serialize_update_log_request_includes_no_more_true() {
        let req = UpdateLogRequest {
            task_id: 1,
            index: 0,
            rows: vec![],
            no_more: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""noMore":true"#));
    }

    #[test]
    fn deserialize_fetch_task_response_no_task() {
        let json = r#"{"tasksVersion":"5"}"#;
        let resp: FetchTaskResponse = serde_json::from_str(json).unwrap();
        assert!(resp.task.is_none());
        assert_eq!(resp.tasks_version, 5);
    }

    #[test]
    fn deserialize_task_with_context() {
        let json = r#"{
            "id": "99",
            "workflowPayload": "dGVzdA==",
            "context": {"github": {"job": "build"}},
            "secrets": {"TOKEN": "abc"},
            "vars": {"MY_VAR": "val"}
        }"#;
        let task: Task = serde_json::from_str(json).unwrap();
        assert_eq!(task.id, 99);
        assert_eq!(task.secrets.get("TOKEN").unwrap(), "abc");
        assert_eq!(task.vars.get("MY_VAR").unwrap(), "val");
        assert_eq!(task.context["github"]["job"].as_str().unwrap(), "build");
    }

    #[test]
    fn deserialize_update_log_response() {
        let json = r#"{"ackIndex":"15"}"#;
        let resp: UpdateLogResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.ack_index, 15);
    }

    #[test]
    fn serialize_fetch_task_request_i64_as_string() {
        let req = FetchTaskRequest { tasks_version: 42 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""tasksVersion":"42""#), "got: {}", json);
    }

    #[test]
    fn serialize_register_request() {
        let req = RegisterRequest {
            name: "my-runner".to_string(),
            token: "tok123".to_string(),
            version: "0.1.0".to_string(),
            labels: vec!["self-hosted:host".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""name":"my-runner""#));
        assert!(json.contains(r#""token":"tok123""#));
        assert!(json.contains(r#""labels":["self-hosted:host"]"#));
    }

    #[test]
    fn timestamp_now_is_reasonable() {
        let ts = Timestamp::now();
        assert!(ts.seconds > 1_700_000_000); // after 2023
        assert!(ts.nanos >= 0);
    }

    #[test]
    fn timestamp_serializes_as_rfc3339() {
        let ts = Timestamp {
            seconds: 1700000000,
            nanos: 500_000_000,
        };
        let json = serde_json::to_string(&ts).unwrap();
        assert!(json.starts_with('"'), "should be a string: {}", json);
        assert!(json.contains("2023-11-14T"), "got: {}", json);
    }

    #[test]
    fn timestamp_deserializes_from_rfc3339() {
        let ts: Timestamp = serde_json::from_str(r#""2023-11-14T22:13:20.500000000Z""#).unwrap();
        assert_eq!(ts.seconds, 1700000000);
        assert_eq!(ts.nanos, 500_000_000);
    }

    #[test]
    fn timestamp_deserializes_from_object() {
        let ts: Timestamp = serde_json::from_str(r#"{"seconds":1700000000,"nanos":0}"#).unwrap();
        assert_eq!(ts.seconds, 1700000000);
    }

    #[test]
    fn serialize_update_task_request_omits_empty_outputs() {
        let req = UpdateTaskRequest {
            state: TaskState {
                id: 1,
                result: TaskResult::Success,
                started_at: None,
                stopped_at: None,
                steps: vec![],
            },
            outputs: HashMap::new(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("outputs"));
    }
}

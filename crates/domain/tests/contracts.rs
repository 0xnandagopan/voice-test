use serde_json::Value;
use v0_domain::workflow::{ProgressCommand, VoiceControlRequest, WorkflowCommand};
#[test]
fn canonical_fixtures_round_trip_and_reject_client_authority_fields() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../../../contracts/workflow.fixtures.json")).unwrap();
    for item in fixtures["workflow"].as_array().unwrap() {
        let parsed: WorkflowCommand = serde_json::from_value(item.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), *item);
        let mut bad = item.clone();
        bad["actor"] = Value::String("operator".into());
        assert!(serde_json::from_value::<WorkflowCommand>(bad).is_err());
    }
    let p: ProgressCommand = serde_json::from_value(fixtures["progress"].clone()).unwrap();
    assert_eq!(serde_json::to_value(p).unwrap(), fixtures["progress"]);
    let v: VoiceControlRequest = serde_json::from_value(fixtures["voice"].clone()).unwrap();
    assert_eq!(serde_json::to_value(v).unwrap(), fixtures["voice"]);
    let mut bad = fixtures["progress"].clone();
    bad["time_consumed_seconds"] = 0.into();
    assert!(serde_json::from_value::<ProgressCommand>(bad).is_err());
}

use super::*;

#[test]
fn sign_in_errors_survive_the_trip_between_processes() {
    let wire: WireError = Error::NeedsAuth("linear".into()).into();
    let back: Error = serde_json::from_value::<WireError>(serde_json::to_value(&wire).unwrap())
        .unwrap()
        .into();
    assert!(matches!(back, Error::NeedsAuth(server) if server == "linear"));
}

use super::*;

/// A family whose production prerequisites are missing must be refused by name.
/// Building it with placeholder credentials would produce a runtime that looks
/// configured and is not, and the failure would surface later as an unauthorised
/// control rather than here as a missing prerequisite.
#[test]
fn sglang_is_refused_by_name_rather_than_stubbed() {
    let CoordinatorError::Service(message) =
        ProfileBindings::missing("SGLang", "its controls need a resolved admin credential")
    else {
        panic!("a missing prerequisite is a service failure");
    };
    assert!(message.contains("SGLang"), "{message}");
    assert!(message.contains("credential"), "{message}");
    assert!(
        message.contains("Refusing"),
        "the refusal must be explicit: {message}"
    );
}

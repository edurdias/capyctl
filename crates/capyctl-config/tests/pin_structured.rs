use capyctl_config::deployment_pin::pin_references;
use serde_json::json;

// T14: pin model and drafter revisions without losing source restrictions.
#[tokio::test]
async fn structured_sources_preserve_files_and_token_references() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let mut document = json!({"model": {
        "source": {"type": "huggingface", "repo": "owner/model", "revision": "release", "files": ["*.gguf"], "token_ref": "secret://hub"},
        "draft": {"huggingface": {"repo": "owner/draft", "files": ["*.json"], "token_ref": "secret://draft"}}
    }});
    let original = document.clone();
    pin_references(&mut document, |_, _, _| async { Ok(sha.to_owned()) })
        .await
        .unwrap();
    let mut expected = original;
    expected["model"]["source"]["revision"] = json!(sha);
    expected["model"]["draft"]["huggingface"]["revision"] = json!(sha);
    assert_eq!(document, expected);
}

// T14: already pinned written-out sources do not contact the hub.
#[tokio::test]
async fn pinned_sources_are_not_resolved_again() {
    let mut document = json!({"model": {"source": {"type": "huggingface", "repo": "owner/model", "revision": "0123456789abcdef0123456789abcdef01234567"}}});
    pin_references(&mut document, |_, _, _| async { panic!("already pinned") })
        .await
        .unwrap();
}

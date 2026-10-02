use capyctl_config::{parse_document, yaml_emit::to_block_yaml};
use serde_json::json;

#[test]
fn emits_block_style_and_parses_back_to_the_same_tree() {
    let value = json!({
        "a": {"b": "1GiB", "c": 4, "d": true, "e": null},
        "empty_map": {},
        "empty_list": [],
        "list": ["x", {"k": "v", "nest": {"m": 1}}],
        "quoted: key": "has \"quotes\" and: colon",
        "path": "/home/me/.local/state/capyctl",
        "float": 0.5
    });
    let text = to_block_yaml(&value);
    assert!(text.contains("a:\n  b: \"1GiB\"\n  c: 4\n"), "{text}");
    assert!(
        text.contains("list:\n  - \"x\"\n  - k: \"v\"\n    nest:\n      m: 1\n"),
        "{text}"
    );
    assert!(text.ends_with('\n'));
    assert_eq!(parse_document(&text).unwrap(), value);
}

//! ADR 0021, ADR 0014 §3, §6, §8, SPEC §8.2, T21: the published flag
//! ownership table (`capyctl engine flags`) says what the checks do. Every
//! row is run through `validate_extra_args` with no approvals and must be
//! refused or accepted as its class says, and every name a check table holds
//! appears in it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use capyctl_config::engine_policy::{
    flag_table, reserved_options, typed_options, validate_extra_args, Engine, ExtraArgsContext,
    FlagClass, NameMatch, ProfileArgError, SGLANG_DUMP_RESERVED,
};

fn check(engine: Engine, option: &str, value: &str) -> Result<(), ProfileArgError> {
    let none = BTreeSet::new();
    let paths: Vec<PathBuf> = Vec::new();
    let context = ExtraArgsContext {
        engine,
        line: Default::default(),
        sleep_mode: false,
        approved_options: &none,
        approved_paths: &paths,
        checkpoint_root: None,
        host_fixed: &none,
    };
    validate_extra_args(&[option.to_owned(), value.to_owned()], &context)
}

// T21
#[test]
fn every_row_is_classed_as_the_checks_class_it() {
    for engine in Engine::ALL {
        let rows = flag_table(engine);
        assert!(!rows.is_empty(), "{engine:?}");
        for row in rows {
            let option = match row.name_match {
                NameMatch::Name => row.name.clone(),
                NameMatch::Prefix => format!("{}x", row.name),
            };
            // ADR 0033 §11: a row about one value is checked with it.
            let result = check(engine, &option, row.value.unwrap_or("1"));
            let agrees = match row.class {
                FlagClass::Owned => matches!(
                    result,
                    Err(ProfileArgError::Reserved(_)
                        | ProfileArgError::ReservedField { .. }
                        | ProfileArgError::TypedField { .. }
                        | ProfileArgError::Lmcache(_))
                ),
                FlagClass::Reserved => matches!(
                    result,
                    Err(ProfileArgError::Reserved(_) | ProfileArgError::ConfigFile(_))
                ),
                FlagClass::Sensitive => {
                    row.approval.is_some() && matches!(result, Err(ProfileArgError::Sensitive(_)))
                }
                FlagClass::Ordinary => result.is_ok(),
            };
            assert!(
                agrees,
                "{engine:?} `{option}` is {:?} in the table, the check says {result:?}",
                row.class
            );
        }
    }
}

// T21
#[test]
fn every_checked_name_is_in_the_table() {
    for engine in Engine::ALL {
        let names: BTreeSet<String> = flag_table(engine).into_iter().map(|r| r.name).collect();
        for name in reserved_options(engine, false) {
            assert!(names.contains(&name), "{engine:?} `{name}` missing");
        }
        for (name, _) in typed_options(engine) {
            assert!(names.contains(*name), "{engine:?} `{name}` missing");
        }
    }
    let sglang = flag_table(Engine::Sglang);
    for (field, _) in SGLANG_DUMP_RESERVED {
        let option = format!("--{}", field.replace('_', "-"));
        let row = sglang
            .iter()
            .find(|row| row.name == option)
            .unwrap_or_else(|| panic!("`{option}` missing"));
        assert_eq!(row.class, FlagClass::Reserved, "{option}");
    }
}

// T21, ADR 0021: the JSON form carries each class and the approval kind.
#[test]
fn the_json_form_names_classes_and_approvals() {
    let json = serde_json::to_value(flag_table(Engine::Sglang)).unwrap();
    let rows = json.as_array().unwrap();
    let classes: BTreeSet<&str> = rows.iter().filter_map(|r| r["class"].as_str()).collect();
    for class in ["owned", "reserved", "sensitive", "ordinary"] {
        assert!(classes.contains(class), "{class}");
    }
    let watermark = rows
        .iter()
        .find(|r| r["name"] == "--watermark-config")
        .unwrap();
    assert_eq!(watermark["class"], "reserved");
    assert_eq!(watermark["match"], "name");
    assert!(watermark.get("approval").is_none());
    let draft = rows
        .iter()
        .find(|r| r["name"] == "--speculative-draft-model-path")
        .unwrap();
    assert_eq!(draft["class"], "sensitive");
    assert_eq!(draft["approval"], "path");
}

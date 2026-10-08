//! ADR 0014 §2, §3, §8 and ADR 0023 §3, §4, T14: the engine option tables in
//! `docs/guide/engine-flags.md` say what the deploy-time policy does.
//!
//! Every option a table names is run through `validate_extra_args` with no
//! host approvals, and the refusal (or acceptance) must match the row's class;
//! a typed row must name the field the refusal names. Every typed option the
//! policy knows must appear in its engine's table, so a typed field added on
//! one side and forgotten on the other fails here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use capyctl_config::engine_policy::{
    typed_options, validate_extra_args, Engine, ExtraArgsContext, ProfileArgError,
};

const PAGE: &str = include_str!("../../../docs/guide/engine-flags.md");

const SECTIONS: &[(&str, Engine)] = &[
    ("## vLLM 0.30", Engine::Vllm),
    ("## SGLang 0.5.21", Engine::Sglang),
    ("## TensorFold 0.6.5", Engine::Tensorfold),
];

const HEADER: &str = "| Need | Option | Class | CapyCTL |";

#[derive(Debug)]
struct Row {
    options: Vec<String>,
    class: String,
    /// The first code span of the CapyCTL column.
    setting: Option<String>,
}

fn code_spans(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The rows of each engine's table, keyed by engine.
fn tables(page: &str) -> BTreeMap<&'static str, (Engine, Vec<Row>)> {
    let mut tables = BTreeMap::new();
    for (heading, engine) in SECTIONS {
        let start = page
            .lines()
            .position(|line| line == *heading)
            .unwrap_or_else(|| panic!("no `{heading}` section"));
        let section: Vec<&str> = page
            .lines()
            .skip(start + 1)
            .take_while(|line| !line.starts_with("## "))
            .collect();
        let header = section
            .iter()
            .position(|line| *line == HEADER)
            .unwrap_or_else(|| panic!("`{heading}` has no `{HEADER}` table"));
        let rows: Vec<Row> = section[header + 2..]
            .iter()
            .take_while(|line| line.starts_with('|'))
            .map(|line| {
                let cells: Vec<&str> = line.trim_matches('|').split(" | ").collect();
                assert_eq!(cells.len(), 4, "{heading}: malformed row `{line}`");
                Row {
                    options: code_spans(cells[1])
                        .into_iter()
                        .filter(|span| span.starts_with("--"))
                        .collect(),
                    class: cells[2].trim().to_owned(),
                    setting: code_spans(cells[3]).into_iter().next(),
                }
            })
            .collect();
        assert!(!rows.is_empty(), "`{heading}` table is empty");
        tables.insert(*heading, (*engine, rows));
    }
    tables
}

/// The class and, for a typed option, the field the policy gives `option`.
fn classify(engine: Engine, option: &str) -> (&'static str, Option<String>) {
    let none = BTreeSet::new();
    let paths: Vec<PathBuf> = Vec::new();
    let context = ExtraArgsContext {
        engine,
        sleep_mode: false,
        approved_options: &none,
        approved_paths: &paths,
        checkpoint_root: None,
        host_fixed: &none,
    };
    match validate_extra_args(&[option.to_owned(), "1".to_owned()], &context) {
        Ok(()) => ("extra", None),
        Err(ProfileArgError::Reserved(_) | ProfileArgError::ConfigFile(_)) => ("reserved", None),
        Err(ProfileArgError::TypedField { field, .. }) => ("typed", Some(field)),
        Err(ProfileArgError::Sensitive(_) | ProfileArgError::PathNotApproved(_)) => {
            ("approval", None)
        }
        Err(other) => panic!("{option}: unexpected refusal {other}"),
    }
}

fn mismatches(page: &str) -> Vec<String> {
    let mut wrong = Vec::new();
    for (heading, (engine, rows)) in tables(page) {
        let mut listed = BTreeSet::new();
        for row in &rows {
            if row.options.is_empty() {
                assert_eq!(
                    row.class, "—",
                    "{heading}: a row without options has no class"
                );
                continue;
            }
            for option in &row.options {
                listed.insert(option.clone());
                let (class, field) = classify(engine, option);
                if class != row.class {
                    wrong.push(format!(
                        "{heading}: `{option}` is {class}, the table says {}",
                        row.class
                    ));
                }
                if let Some(field) = field {
                    let documented = row
                        .setting
                        .as_deref()
                        .and_then(|setting| setting.strip_prefix("engine_config."));
                    if documented != Some(field.as_str()) {
                        wrong.push(format!(
                            "{heading}: `{option}` is typed field `engine_config.{field}`, \
                             the table names {:?}",
                            row.setting
                        ));
                    }
                }
            }
        }
        for (option, field) in typed_options(engine) {
            if !listed.contains(*option) {
                wrong.push(format!(
                    "{heading}: typed option `{option}` (`engine_config.{field}`) is missing"
                ));
            }
        }
    }
    wrong
}

// T14
#[test]
fn the_engine_option_tables_match_the_policy() {
    let wrong = mismatches(PAGE);
    assert!(
        wrong.is_empty(),
        "docs/guide/engine-flags.md:\n{}",
        wrong.join("\n")
    );
}

/// The check catches a wrong class, a wrong typed field and a missing typed
/// option, so the tables cannot drift from the policy unnoticed.
// T14
#[test]
fn a_wrong_row_fails_the_check() {
    let wrong_class = PAGE.replacen(
        "| `--mem-fraction-static` | reserved |",
        "| `--mem-fraction-static` | extra |",
        1,
    );
    assert_ne!(wrong_class, PAGE);
    assert!(mismatches(&wrong_class)
        .iter()
        .any(|m| m.contains("`--mem-fraction-static` is reserved")));

    let wrong_field = PAGE.replacen(
        "| `--max-num-seqs` | typed | `engine_config.max_concurrent_requests` |",
        "| `--max-num-seqs` | typed | `engine_config.context_length` |",
        1,
    );
    assert_ne!(wrong_field, PAGE);
    assert!(mismatches(&wrong_field)
        .iter()
        .any(|m| m.contains("`--max-num-seqs` is typed field")));

    let missing = PAGE.replacen(
        "| KV cache dtype | `--kv-dtype` | typed |",
        "| KV cache dtype | none | — |",
        1,
    );
    assert_ne!(missing, PAGE);
    assert!(mismatches(&missing)
        .iter()
        .any(|m| m.contains("typed option `--kv-dtype`")));
}

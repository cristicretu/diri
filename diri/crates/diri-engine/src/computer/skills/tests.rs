use std::cell::RefCell;
use std::collections::VecDeque;

use serde_json::{Value, json};

use super::compile::{check_args, compile, substitute};
use super::recorder::{Learning, apply_diff, parse_rows, selector};
use super::*;

const PROBE: &str = "dev.diri.probe";

fn wrap(structured: Value) -> Value {
    json!({"mcpToolResult": {"content": [{"type": "text", "text": "ok"}], "structuredContent": structured}})
}

fn batch_ok(steps: usize) -> Value {
    wrap(
        json!({"ok": true, "total": steps, "executed": steps, "failed_step": null,
                "steps": vec![json!({"ok": true}); steps]}),
    )
}

fn batch_failed(at: usize, message: &str) -> Value {
    let mut steps = vec![json!({"ok": true}); at];
    steps.push(json!({"ok": false, "phase": "find", "code": "not_found", "message": message}));
    json!({"mcpToolResult": {"isError": true,
        "content": [{"type": "text", "text": format!("run_steps: step {} failed", at + 1)}],
        "structuredContent": {"ok": false, "failed_step": at, "steps": steps}}})
}

/// A scripted driver: answers lookups, and `run_steps` from a queue (ok by
/// default), remembering every call.
struct Fake {
    calls: RefCell<Vec<(String, Value)>>,
    batches: RefCell<VecDeque<Value>>,
    version: RefCell<Option<String>>,
    running: RefCell<bool>,
}

impl Fake {
    fn new() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            batches: RefCell::new(VecDeque::new()),
            version: RefCell::new(Some("1.0".into())),
            running: RefCell::new(true),
        }
    }

    fn batches(&self) -> Vec<Value> {
        self.calls
            .borrow()
            .iter()
            .filter(|(tool, _)| tool == "run_steps")
            .map(|(_, args)| args.clone())
            .collect()
    }
}

impl Actor for Fake {
    fn call(&self, tool: &str, args: Value) -> Result<Value, ComputerError> {
        self.calls
            .borrow_mut()
            .push((tool.to_owned(), args.clone()));
        Ok(match tool {
            "list_windows" if *self.running.borrow() => wrap(
                json!({"windows": [{"pid": 77, "window_id": 5, "is_on_screen": true, "title": "Probe", "app_name": "Probe"}]}),
            ),
            "list_windows" => wrap(json!({"windows": []})),
            "launch_app" => {
                *self.running.borrow_mut() = true;
                wrap(json!({"pid": 77}))
            }
            "run_steps" => {
                let steps = args["steps"].as_array().map_or(0, Vec::len);
                self.batches
                    .borrow_mut()
                    .pop_front()
                    .unwrap_or_else(|| batch_ok(steps))
            }
            "get_window_state" => wrap(json!({"snapshot_id": "s9", "app_name": "Probe",
                "window_title": "Probe",
                "tree_markdown": "- [0] AXWindow \"Probe\"\n  - [1] AXButton \"Add Item\""})),
            _ => wrap(json!({})),
        })
    }

    fn bundle_info(&self, _pid: i64) -> Option<BundleInfo> {
        Some(BundleInfo {
            bundle_id: Some(PROBE.into()),
            name: Some("Probe".into()),
            version: self.version.borrow().clone(),
        })
    }
}

fn live(_: &str) -> bool {
    true
}

fn skills() -> (tempfile::TempDir, AppSkills) {
    let temp = tempfile::tempdir().unwrap();
    let skills = AppSkills::new(temp.path().join("app-skills"));
    (temp, skills)
}

fn add_item_skill() -> Value {
    json!({
        "name": "add_item",
        "app": {"bundle_id": PROBE, "name": "Probe"},
        "goal": "Add an item with a title",
        "params": [{"name": "title", "type": "string", "description": "Item title"}],
        "steps": [
            {"tool": "click", "args": {"role": "button", "name": "Add Item"}, "intent": "Open the new item form"},
            {"set_value": {"role": "textfield", "name": "Title", "value": "{title}"}, "intent": "Type the title"},
            {"tool": "click", "args": {"role": "button", "name": "Save"}, "intent": "Save the item",
             "expect": [{"text": "{title}"}]}
        ]
    })
}

fn save(skills: &AppSkills, fake: &Fake, skill: Value) -> Value {
    let args = json!({"skill": skill, "example_args": {"title": "Milk"}});
    skills
        .handle(Some(fake), Some("s1"), "save", &args, &live)
        .unwrap()
}

fn run(skills: &AppSkills, fake: &Fake, args: Value) -> Result<Value, ComputerError> {
    skills.handle(Some(fake), Some("s1"), "run", &args, &live)
}

fn obj(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[test]
fn paths_are_validated_as_components() {
    use super::store::{valid_bundle_id, valid_skill_name};
    assert!(valid_bundle_id("com.apple.TextEdit"));
    assert!(valid_bundle_id("dev.diri.probe-2"));
    for bad in ["", "..", "../x", "a/b", ".hidden", "a..b", "a b", "x\0"] {
        assert!(!valid_bundle_id(bad), "{bad:?}");
    }
    assert!(valid_skill_name("create_invoice2"));
    let long = "a".repeat(65);
    for bad in ["", "Create", "2go", "a-b", "../x", "a.b", long.as_str()] {
        assert!(!valid_skill_name(bad), "{bad:?}");
    }
    let (_temp, skills) = skills();
    let error = skills
        .handle(
            None,
            None,
            "history",
            &json!({"app": "../etc", "skill": "x"}),
            &live,
        )
        .unwrap_err();
    assert_eq!(error.code, SKILL_NOT_FOUND);
    assert!(skills.store.load("../etc", "x").is_err());
    assert!(skills.store.load(PROBE, "../x").is_err());
}

#[test]
fn a_saved_skill_round_trips_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_temp, skills) = skills();
    let fake = Fake::new();
    let reply = save(&skills, &fake, add_item_skill());
    assert_eq!(reply["saved"], true, "{reply}");
    assert_eq!(reply["status"], "verified");
    assert_eq!(reply["version"], 1);

    let stored = skills.store.load(PROBE, "add_item").unwrap().unwrap();
    assert_eq!(stored.steps.len(), 3);
    // The `{set_value: ...}` shape is stored canonically.
    assert_eq!(stored.steps[1]["tool"], "set_value");
    assert_eq!(stored.steps[1]["args"]["value"], "{title}");
    assert_eq!(stored.app_version.as_deref(), Some("1.0"));

    let root = skills.store.root();
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(root), 0o700);
    assert_eq!(mode(&root.join(PROBE)), 0o700);
    assert_eq!(mode(&root.join(PROBE).join("skills")), 0o700);
    assert_eq!(mode(&root.join(PROBE).join("skills/add_item.json")), 0o600);
    assert_eq!(mode(&root.join(PROBE).join("app.json")), 0o600);
    assert_eq!(mode(&root.join(PROBE).join("runs.jsonl")), 0o600);

    // The run log never holds argument values.
    let log = std::fs::read_to_string(root.join(PROBE).join("runs.jsonl")).unwrap();
    assert!(log.contains("verify_ok"), "{log}");
    assert!(!log.contains("Milk"), "{log}");

    let list = skills.list();
    assert_eq!(list.apps.len(), 1);
    assert_eq!(list.apps[0].name, "Probe");
    assert_eq!(list.apps[0].skills[0].params, ["title"]);
    let listed = skills
        .handle(None, None, "list", &json!({"app": "probe"}), &live)
        .unwrap();
    assert_eq!(listed["apps"][0]["skills"][0]["name"], "add_item");
}

#[test]
fn a_symlinked_app_directory_is_refused() {
    let (temp, skills) = skills();
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::create_dir_all(temp.path().join("app-skills")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, temp.path().join("app-skills").join(PROBE)).unwrap();
    assert!(skills.store.load(PROBE, "add_item").is_err());
}

#[test]
fn placeholders_substitute_and_escape() {
    let args = obj(json!({"title": "Milk", "count": 3, "urgent": true}));
    assert_eq!(
        substitute(&json!("Buy {title}!"), &args),
        json!("Buy Milk!")
    );
    assert_eq!(
        substitute(&json!("{{title}} is {title}"), &args),
        json!("{title} is Milk")
    );
    assert_eq!(substitute(&json!("{count}"), &args), json!(3));
    assert_eq!(substitute(&json!("{urgent}"), &args), json!(true));
    assert_eq!(substitute(&json!("x{count}"), &args), json!("x3"));
    assert_eq!(
        substitute(&json!({"keys": ["cmd", "{title}"], "n": 1}), &args),
        json!({"keys": ["cmd", "Milk"], "n": 1})
    );

    let params = vec![
        Param {
            name: "title".into(),
            kind: ParamKind::String,
            description: String::new(),
        },
        Param {
            name: "count".into(),
            kind: ParamKind::Number,
            description: String::new(),
        },
    ];
    assert!(check_args(&params, &json!({"title": "a", "count": 2})).is_ok());
    let error = check_args(&params, &json!({"title": 5, "extra": 1})).unwrap_err();
    assert!(error.contains("missing count"), "{error}");
    assert!(error.contains("title must be a string"), "{error}");
    assert!(error.contains("extra is not a param"), "{error}");
}

#[test]
fn validation_names_every_problem() {
    let params = vec![
        Param {
            name: "title".into(),
            ..Param::default()
        },
        Param {
            name: "unused".into(),
            ..Param::default()
        },
    ];
    let steps = vec![
        json!({"tool": "click", "args": {"element_token": "s1:4"}}),
        json!({"tool": "launch_app", "args": {"bundle_id": "x"}}),
        json!({"tool": "click", "args": {"x": 10, "y": 20}}),
        json!({"tool": "type_text", "args": {"text": "{title} {missing}", "pid": 4}}),
        json!({"tool": "click", "args": {"role": "button", "name": "a {b"}}),
        json!({"tool": "press_key", "args": {"key": "return", "delivery_mode": "foreground"}}),
        json!({"bogus": 1}),
    ];
    let problems = compile(&params, &steps, &[], &[]).err().unwrap().join("\n");
    for needle in [
        "step 1 uses element_token",
        "step 2: launch_app cannot run in a skill",
        "step 3 clicks raw pixels",
        "step 4 uses pid",
        "{missing} is used but not declared",
        "\"a {b\" has a brace",
        "step 6 asks for the foreground",
        "step 7: unknown key",
        "param unused is declared but no step uses it",
    ] {
        assert!(problems.contains(needle), "{needle} in\n{problems}");
    }
    let many = vec![json!({"tool": "press_key", "args": {"key": "tab"}}); 65];
    assert!(compile(&[], &many, &[], &[]).err().unwrap()[0].contains("at most 64"));
    // A pixel step that says so is allowed.
    let pixel = [json!({"tool": "click", "args": {"x": 1, "y": 2}, "pixel": true})];
    assert!(compile(&[], &pixel, &[], &[]).is_ok());
}

#[test]
fn tokens_become_selectors_from_the_read_they_came_from() {
    let rows = parse_rows(
        "- [0] AXWindow \"Probe\"\n  - [1] AXButton \"Delete\"\n  - [2] AXTextField = \"Milk\" (Title)\n  - [3] AXButton \"Delete\"\n  - [4] AXGroup",
    );
    assert_eq!(
        Value::Object(selector(&rows, 1).unwrap()),
        json!({"role": "button", "name": "Delete", "nth": 0})
    );
    assert_eq!(
        Value::Object(selector(&rows, 3).unwrap()),
        json!({"role": "button", "name": "Delete", "nth": 1})
    );
    assert_eq!(
        Value::Object(selector(&rows, 2).unwrap()),
        json!({"role": "textfield", "name": "Title"})
    );
    assert!(
        selector(&rows, 4).is_none(),
        "an unlabeled row has no selector"
    );

    // A diff read moves rows; the merged rows follow.
    let moved = apply_diff(
        rows,
        "since s1: 1 added, 0 changed, 1 removed.\n+ [1] AXButton \"Add Item\"\n- [1] AXButton \"Delete\"\nreindexed: [2-4] -> [3-5]\n",
    );
    assert_eq!(moved[&1].label, "Add Item");
    assert_eq!(moved[&3].label, "Title");
    assert_eq!(moved[&4].label, "Delete");

    // Recorded end to end: a read, then token actions and a run_steps.
    let mut learning = Learning::new(
        AppRef {
            bundle_id: PROBE.into(),
            name: "Probe".into(),
        },
        "Add an item".into(),
    );
    let read = wrap(json!({"snapshot_id": "s1", "window_title": "Probe",
        "tree_markdown": "- [0] AXWindow \"Probe\"\n  - [1] AXButton \"Add Item\"\n  - [2] AXTextField \"Title\""}));
    learning.record(
        "get_window_state",
        &obj(json!({"pid": 77, "window_id": 5})),
        Some(&read),
    );
    learning.record(
        "click",
        &obj(json!({"element_token": "s1:1", "pid": 77, "session": "x"})),
        Some(&wrap(json!({}))),
    );
    learning.record("list_windows", &obj(json!({})), Some(&wrap(json!({}))));
    let refused = json!({"mcpToolResult": {"isError": true}});
    learning.record(
        "click",
        &obj(json!({"element_token": "s1:9"})),
        Some(&refused),
    );
    learning.record(
        "run_steps",
        &obj(json!({"steps": [
            {"type_text": {"element_token": "s1:2", "text": "Milk"}},
            {"tool": "press_key", "args": {"key": "return", "pid": 77}},
            {"tool": "click", "args": {"role": "button", "name": "Never ran"}}
        ]})),
        Some(&wrap(
            json!({"steps": [{"ok": true}, {"ok": true}, {"ok": false}]}),
        )),
    );
    let trace = learning.trace();
    let steps = trace["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3, "{trace}");
    assert_eq!(
        steps[0]["args"],
        json!({"role": "button", "name": "Add Item"})
    );
    assert_eq!(steps[0]["intent"], "click button \"Add Item\"");
    assert_eq!(steps[1]["tool"], "type_text");
    assert_eq!(
        steps[1]["args"],
        json!({"role": "textfield", "name": "Title", "text": "Milk"})
    );
    assert_eq!(steps[2]["args"], json!({"key": "return"}));
    assert_eq!(
        trace["candidate_params"][0],
        json!({"step": 2, "field": "text", "literal": "Milk"})
    );
    assert_eq!(trace["dropped"]["reads"], 2);
    assert_eq!(trace["dropped"]["failed_actions"], 2);
    // The intent never repeats typed text.
    assert!(!steps[1]["intent"].as_str().unwrap().contains("Milk"));
}

#[test]
fn the_recorder_is_off_unless_the_session_is_learning() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    skills.observe(
        "s1",
        "click",
        &obj(json!({"element_token": "s1:1"})),
        Some(&wrap(json!({}))),
    );
    assert!(!skills.is_learning("s1"));
    assert_eq!(skills.learning.load(Ordering::Acquire), 0);
    assert!(
        skills
            .handle(Some(&fake), Some("s1"), "trace", &json!({}), &live)
            .is_err()
    );

    let learned = skills
        .handle(
            Some(&fake),
            Some("s1"),
            "learn",
            &json!({"app": "Probe", "goal": "Add an item"}),
            &live,
        )
        .unwrap();
    assert_eq!(learned["app"]["bundle_id"], PROBE);
    assert!(skills.is_learning("s1"));
    assert!(!skills.is_learning("s2"));
    let tab = obj(json!({"key": "tab"}));
    skills.observe("s1", "press_key", &tab, Some(&wrap(json!({}))));
    skills.observe("s2", "press_key", &tab, Some(&wrap(json!({}))));
    let trace = skills
        .handle(Some(&fake), Some("s1"), "trace", &json!({}), &live)
        .unwrap();
    assert_eq!(trace["steps"].as_array().unwrap().len(), 1);

    // The session ended: the recording goes with it.
    skills.end_session("s1");
    assert!(!skills.is_learning("s1"));
    assert_eq!(skills.learning.load(Ordering::Acquire), 0);

    // Saving ends learn mode too; a dead session's recording is pruned.
    let learn = json!({"app": PROBE, "goal": "x"});
    skills
        .handle(Some(&fake), Some("s1"), "learn", &learn, &live)
        .unwrap();
    save(&skills, &fake, add_item_skill());
    assert!(!skills.is_learning("s1"));
    skills
        .handle(Some(&fake), Some("s3"), "learn", &learn, &live)
        .unwrap();
    skills
        .handle(None, None, "list", &json!({}), &|id| id != "s3")
        .unwrap();
    assert!(!skills.is_learning("s3"));
}

#[test]
fn verification_failure_saves_nothing() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    fake.batches
        .borrow_mut()
        .push_back(batch_failed(2, "no button named \"Save\""));
    let reply = save(&skills, &fake, add_item_skill());
    assert_eq!(reply["saved"], false);
    assert_eq!(reply["failed_step"], 2);
    assert_eq!(reply["intent"], "Save the item");
    assert_eq!(reply["phase"], "find");
    assert!(reply["driver_error"].as_str().unwrap().contains("Save"));
    assert!(skills.store.load(PROBE, "add_item").unwrap().is_none());
    // The verifying replay used the example args and stripped Diri's keys.
    let sent = &fake.batches()[0];
    assert_eq!(
        sent["app"], "Probe",
        "batches find the window by its app name"
    );
    assert_eq!(sent["steps"][1]["args"]["value"], "Milk");
    assert!(sent["steps"][0].get("intent").is_none());
    assert_eq!(sent["steps"][2]["expect"][0]["text"], "Milk");
}

#[test]
fn long_skills_run_in_chunks_and_lists_report_per_item() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    let mut skill = add_item_skill();
    let mut steps = vec![
        json!({"tool": "set_value", "args": {"role": "textfield", "name": "Title", "value": "{title}"}}),
    ];
    steps.extend((0..24).map(|_| json!({"tool": "press_key", "args": {"key": "tab"}})));
    skill["steps"] = json!(steps);
    skill["preconditions"] = json!([{"role": "button", "name": "Add Item"}]);
    save(&skills, &fake, skill);
    let batches = fake.batches();
    assert_eq!(batches.len(), 2, "26 steps = 20 + 6");
    assert_eq!(batches[0]["steps"].as_array().unwrap().len(), 20);
    assert_eq!(
        batches[0]["steps"][0],
        json!({"wait_for": {"role": "button", "name": "Add Item"}})
    );
    assert_eq!(batches[1]["steps"].as_array().unwrap().len(), 6);
    fake.calls.borrow_mut().clear();

    // Item 1 fails in the second chunk, at skill step 21 (0-based).
    fake.batches.borrow_mut().extend([
        batch_ok(20),
        batch_ok(6),
        batch_ok(20),
        batch_failed(2, "no field"),
        batch_ok(20),
        batch_ok(6),
    ]);
    let reply = run(
        &skills,
        &fake,
        json!({"app": "Probe", "skill": "add_item", "continue_on_error": true,
               "args_list": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
    )
    .unwrap();
    assert_eq!(reply["ok"], false);
    let items = reply["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["ok"], true);
    assert_eq!(items[1]["ok"], false);
    assert_eq!(items[1]["failed_step"], 21);
    assert_eq!(items[2]["ok"], true);
    assert_eq!(reply["first_failure"]["item"], 1);
    assert!(
        reply["first_failure"]["window"]["tree"]
            .as_str()
            .unwrap()
            .contains("Add Item")
    );
    assert!(reply["first_failure"]["saved_skill"]["steps"].is_array());

    // Without continue_on_error the rest is reported as not run.
    fake.batches.borrow_mut().extend([batch_failed(0, "gone")]);
    let reply = run(
        &skills,
        &fake,
        json!({"app": PROBE, "skill": "add_item", "args_list": [{"title": "a"}, {"title": "b"}]}),
    )
    .unwrap();
    assert_eq!(reply["not_run"], json!([1]));
    assert_eq!(reply["first_failure"]["failed"], "precondition");
}

#[test]
fn three_failures_in_a_row_mark_a_skill_broken_and_a_clean_run_heals_it() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    save(&skills, &fake, add_item_skill());
    let call = json!({"app": PROBE, "skill": "add_item", "args": {"title": "Eggs"}});
    for attempt in 1..=3 {
        fake.batches
            .borrow_mut()
            .push_back(batch_failed(0, "no button named \"Add Item\""));
        let reply = run(&skills, &fake, call.clone()).unwrap();
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["consecutive_failures"], attempt);
        assert_eq!(reply["intent"], "Open the new item form");
        assert_eq!(reply["goal"], "Add an item with a title");
        assert!(
            reply["repair"]
                .as_str()
                .unwrap()
                .contains("don't overwrite")
        );
        assert_eq!(
            reply["status"],
            if attempt == 3 { "broken" } else { "verified" },
            "attempt {attempt}"
        );
    }
    let stored = skills.store.load(PROBE, "add_item").unwrap().unwrap();
    assert_eq!(stored.stats.failures, 3);
    assert_eq!(stored.stats.runs, 3);
    let log = std::fs::read_to_string(skills.store.root().join(PROBE).join("runs.jsonl")).unwrap();
    assert!(!log.contains("Eggs"), "{log}");

    let reply = run(&skills, &fake, call).unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["status"], "verified");
    let stored = skills.store.load(PROBE, "add_item").unwrap().unwrap();
    assert_eq!(stored.stats.consecutive_failures, 0);
    assert!(stored.stats.last_ok_at.is_some());
}

#[test]
fn saving_again_archives_versions_and_rollback_restores_one() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    for round in 0..8 {
        let mut skill = add_item_skill();
        skill["goal"] = json!(format!("Add an item, round {round}"));
        let reply = save(&skills, &fake, skill);
        assert_eq!(reply["version"], round + 1);
    }
    let mut archived = skills.store.archived_versions(PROBE, "add_item").unwrap();
    archived.sort_unstable();
    assert_eq!(
        archived,
        [3, 4, 5, 6, 7],
        "the newest five old versions are kept"
    );

    let history = skills
        .handle(
            None,
            None,
            "history",
            &json!({"app": PROBE, "skill": "add_item"}),
            &live,
        )
        .unwrap();
    assert_eq!(history["versions"].as_array().unwrap().len(), 6);
    assert_eq!(history["versions"][5]["current"], true);

    let reply = skills
        .handle(
            None,
            None,
            "rollback",
            &json!({"app": PROBE, "skill": "add_item", "version": 4}),
            &live,
        )
        .unwrap();
    assert_eq!(reply["version"], 9);
    let current = skills.store.load(PROBE, "add_item").unwrap().unwrap();
    assert_eq!(current.goal, "Add an item, round 3");
    assert_eq!(current.status, AppSkillStatus::Unverified);

    let gone = skills
        .handle(
            None,
            None,
            "forget",
            &json!({"app": "Probe", "skill": "add_item"}),
            &live,
        )
        .unwrap();
    assert_eq!(gone["forgotten"], "add_item");
    assert!(!skills.store.root().join(PROBE).exists());
    assert!(skills.list().apps.is_empty());
}

#[test]
fn an_app_update_marks_skills_unverified_until_a_clean_run() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    save(&skills, &fake, add_item_skill());
    *fake.version.borrow_mut() = Some("2.0".into());
    let call = json!({"app": PROBE, "skill": "add_item", "args": {"title": "x"}});

    fake.batches
        .borrow_mut()
        .push_back(batch_failed(1, "no field named Title"));
    let reply = run(&skills, &fake, call.clone()).unwrap();
    assert_eq!(reply["status"], "unverified");
    assert!(
        reply["drift"].as_str().unwrap().contains("1.0 → 2.0"),
        "{reply}"
    );
    let stored = skills.store.load(PROBE, "add_item").unwrap().unwrap();
    assert_eq!(stored.app_version.as_deref(), Some("1.0"));

    let reply = run(&skills, &fake, call).unwrap();
    assert_eq!(reply["ok"], true);
    let stored = skills.store.load(PROBE, "add_item").unwrap().unwrap();
    assert_eq!(stored.status, AppSkillStatus::Verified);
    assert_eq!(stored.app_version.as_deref(), Some("2.0"));
    let app = skills.store.app(PROBE).unwrap().unwrap();
    assert_eq!(app.versions_seen, ["1.0", "2.0"]);
}

#[test]
fn a_stopped_app_is_launched_in_the_background_first() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    save(&skills, &fake, add_item_skill());
    *fake.running.borrow_mut() = false;
    fake.calls.borrow_mut().clear();
    run(
        &skills,
        &fake,
        json!({"app": PROBE, "skill": "add_item", "args": {"title": "x"}}),
    )
    .unwrap();
    let calls = fake.calls.borrow();
    let launch = calls.iter().find(|(tool, _)| tool == "launch_app").unwrap();
    assert_eq!(launch.1, json!({"bundle_id": PROBE}));
}

#[test]
fn destructive_skills_verify_until_the_last_step_and_need_confirm_for_batches() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    let mut skill = add_item_skill();
    skill["destructive"] = json!(true);
    let reply = save(&skills, &fake, skill);
    assert_eq!(reply["saved"], true);
    assert!(
        reply["note"].as_str().unwrap().contains("Save the item"),
        "{reply}"
    );
    let sent = &fake.batches()[0];
    assert_eq!(
        sent["steps"].as_array().unwrap().len(),
        2,
        "the final step is not run"
    );

    let batch =
        json!({"app": PROBE, "skill": "add_item", "args_list": [{"title": "a"}, {"title": "b"}]});
    fake.calls.borrow_mut().clear();
    let refused = run(&skills, &fake, batch.clone()).unwrap_err();
    assert_eq!(refused.code, SKILL_CONFIRM);
    assert!(refused.message.contains("Ask the person"));
    assert!(fake.batches().is_empty(), "nothing ran");

    let mut confirmed = batch;
    confirmed["confirm"] = json!(true);
    assert_eq!(run(&skills, &fake, confirmed).unwrap()["ok"], true);
    // One item needs no confirmation.
    run(
        &skills,
        &fake,
        json!({"app": PROBE, "skill": "add_item", "args": {"title": "a"}}),
    )
    .unwrap();
}

#[test]
fn bad_run_arguments_run_nothing() {
    let (_temp, skills) = skills();
    let fake = Fake::new();
    save(&skills, &fake, add_item_skill());
    fake.calls.borrow_mut().clear();
    let error = run(
        &skills,
        &fake,
        json!({"app": PROBE, "skill": "add_item", "args_list": [{"title": "a"}, {"title": 3}]}),
    )
    .unwrap_err();
    assert_eq!(error.code, SKILL_INVALID);
    assert!(error.message.contains("item 1"), "{}", error.message);
    assert!(fake.batches().is_empty());
    let missing = run(&skills, &fake, json!({"app": PROBE, "skill": "nope"})).unwrap_err();
    assert_eq!(missing.code, SKILL_NOT_FOUND);
    let no_session = skills
        .handle(
            None,
            None,
            "run",
            &json!({"app": PROBE, "skill": "add_item"}),
            &live,
        )
        .unwrap_err();
    assert_eq!(no_session.code, SKILL_NEEDS_SESSION);
}

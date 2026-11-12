use super::actor::*;
use super::detect::*;
use super::qr::*;
use super::quiz::*;
use super::rollcall::*;
use super::task_group::*;
use super::*;
use crate::tronclass::quiz::Answer;
use crate::tronclass::rollcall::{RollcallKind, SignOutcome};
use crate::tronclass::teacher_qr;
use crate::tronclass::Endpoints;
use crate::wire::{AttemptState, Event};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

fn test_plan_tx() -> watch::Sender<MonitorPlan> {
    watch::channel(MonitorPlan {
        generation: 1,
        routes: Vec::new(),
    })
    .0
}
fn leaked_plan_tx() -> &'static watch::Sender<MonitorPlan> {
    Box::leak(Box::new(test_plan_tx()))
}
fn leaked_plan_rx() -> watch::Receiver<MonitorPlan> {
    leaked_plan_tx().subscribe()
}
fn leaked_qr_rev_tx() -> &'static watch::Sender<u64> {
    Box::leak(Box::new(watch::channel(0u64).0))
}
fn leaked_qr_rev_rx() -> watch::Receiver<u64> {
    leaked_qr_rev_tx().subscribe()
}

#[test]
fn production_quiz_prepared_emitter_matches_v1_contract() {
    let fixture: Value = serde_json::from_str(include_str!("../assets/quiz_prepared_v1.json"))
        .expect("valid shared fixture");
    let fixture_attempt = &fixture["per_account"][0];
    let subjects = fixture_attempt["questions"]
        .as_array()
        .expect("fixture questions")
        .iter()
        .map(|question| {
            json!({
                "id": question["subject_id"],
                "parent_id": question["parent_id"],
                "type": question["type"],
                "answer_type": question["answer_type"],
                "description": question["stem"],
                "options": question["options"],
            })
        })
        .collect::<Vec<_>>();
    let generated_answers = fixture_attempt["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|question| {
            let id = question["subject_id"].as_str().unwrap().to_string();
            let answer = serde_json::from_value::<AnswerWire>(question["answer"].clone())
                .unwrap()
                .into_answer()
                .unwrap();
            (id, answer)
        })
        .collect();
    let existing_answers = fixture_attempt["questions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|question| {
            serde_json::from_value::<AnswerWire>(question["existing_answer"].clone())
                .ok()
                .and_then(|wire| wire.into_answer().ok())
                .map(|answer| (question["subject_id"].as_str().unwrap().to_string(), answer))
        })
        .collect();
    let mut attempts = HashMap::new();
    attempts.insert(
        "a1".to_string(),
        PerAccountAttempt {
            state: AttemptState::Ready,
            prepare_generation: 1,
            prepare_at: Instant::now(),
            prepare_deadline: None,
            answer_contract: None,
            instance_id: "attempt-a1".to_string(),
            subjects,
            generated_answers,
            existing_answers,
            overrides: HashMap::new(),
            conflicts: HashSet::from(["1".to_string()]),
            submitted_subjects: HashSet::new(),
        },
    );
    let activity = QuizActivity {
        activity_token: "fixture-quiz-prepared-v1".to_string(),
        source: Source::Exam,
        course: "行銷管理".to_string(),
        course_id: "55379".to_string(),
        activity_id: "32877".to_string(),
        stem: String::new(),
        attempts,
        countdown_deadline: None,
        held: false,
        discarded: false,
        sources: HashSet::new(),
        plan_generation: 0,
        mutation_blocked: false,
    };
    let emitted = serde_json::to_value(Event::from(quiz_prepared(&activity))).unwrap();
    for field in [
        "event",
        "schema_version",
        "activity_token",
        "quiz_id",
        "activity",
        "course",
    ] {
        assert_eq!(emitted.get(field), fixture.get(field), "field {field}");
    }
    assert_eq!(emitted["conflict_count"], 1);
    let emitted_account = &emitted["per_account"][0];
    assert_eq!(emitted_account["account_id"], fixture_attempt["account_id"]);
    assert_eq!(
        emitted_account["instance_id"],
        fixture_attempt["instance_id"]
    );
    assert_eq!(emitted_account["state"], "ready");
    assert_eq!(emitted_account["questions"], fixture_attempt["questions"]);
    assert_eq!(
        emitted["expected_accounts"],
        json!([{ "account_id": "a1", "state": "ready" }])
    );
}

#[test]
fn a_time_field_is_an_epoch_or_iso_text_and_absent_is_not_invalid() {
    let read = |activity: Value| TimeField::read(&activity, "end_time");
    assert_eq!(
        read(json!({ "end_time": 1_609_459_200_i64 })),
        TimeField::At(1_609_459_200)
    );
    assert_eq!(
        read(json!({ "end_time": "2021-01-01T00:00:00Z" })),
        TimeField::At(1_609_459_200)
    );
    for absent in [
        json!({}),
        json!({ "end_time": null }),
        json!({ "end_time": "  " }),
    ] {
        assert_eq!(read(absent), TimeField::Absent);
    }
    assert_eq!(
        read(json!({ "end_time": "not-a-date" })),
        TimeField::Invalid
    );
}

#[test]
fn exam_answerable_gates_iso_expiry_and_absent_started() {
    let now = 1_700_000_000;
    // started, open, future end → answerable.
    assert!(exam_answerable(
        &json!({"is_started": true, "end_time": "2099-01-01T00:00:00Z"}),
        now
    ));
    // a PAST ISO end_time → not answerable even though is_closed is false (the bug this fixes).
    assert!(!exam_answerable(
        &json!({"is_started": true, "is_closed": false, "end_time": "2000-01-01T00:00:00Z"}),
        now
    ));
    // absent is_started → v1 treats as not-started → skip.
    assert!(!exam_answerable(
        &json!({"end_time": "2099-01-01T00:00:00Z"}),
        now
    ));
    // absent end_time → not past → answerable.
    assert!(exam_answerable(&json!({"is_started": true}), now));
    // A present but malformed deadline must fail closed; it must not be treated like absence.
    assert!(!exam_answerable(
        &json!({"is_started": true, "end_time": "2023-02-29T00:00:00Z"}),
        now
    ));
    assert!(!exam_answerable(
        &json!({"is_started": true, "end_time": "2024-01-01T24:00:00+0800"}),
        now
    ));
}

extern "C" fn noop_cb(_: *const u8, _: usize) {}

// Capture every event emitted through the seam on THIS test thread (each #[tokio::test] runs
// its own current-thread runtime on its own thread, so the thread-local never cross-talks).
thread_local! {
    static EVENTS: std::cell::RefCell<Vec<Value>> = const { std::cell::RefCell::new(Vec::new()) };
}
extern "C" fn record_cb(ptr: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        EVENTS.with(|events| events.borrow_mut().push(v));
    }
}
fn take_events() -> Vec<Value> {
    EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}

fn cfg_countdown(secs: u64) -> MonitorConfig {
    MonitorConfig {
        countdown_secs: secs,
        gate_percent: 15.0,
        llm_endpoint: String::new(),
        llm_model: String::new(),
        llm_key: None,
        llm_max_tokens: 0,
        max_answer_reask: 0,
        prepare_retry_budget_secs: 0,
        autoanswer_types: vec![],
        enable_llm_tools: false,
        max_tool_iterations: 0,
        resubmit_for_correct: false,
        radar_strategy: vec![],
        number_cooldown_ms: 0,
        number_max_cooldowns: 0,
        poll_idle_secs: 5,
        quiz_detect_secs: 45,
        qr_remote_enabled: false,
        qr_remote_base_url: "https://api.hlp.qzz.io".to_string(),
        qr_remote_key: None,
    }
}

/// The surroundings of a handler under test, assembled from the pieces the test holds on to.
fn env_from(
    cb: EventCb,
    cfg: &MonitorConfig,
    accounts: &HashMap<String, Arc<Account>>,
    tx: &UnboundedSender<MonitorMsg>,
    group: &Arc<TaskGroup>,
) -> Env {
    let (runtime_tx, _) = unbounded_channel();
    let plan = MonitorPlan {
        generation: 1,
        routes: Vec::new(),
    };
    let mut env = Env::new(cb, cfg.clone(), plan, tx.clone(), group.clone(), runtime_tx);
    env.accounts = accounts.clone();
    env
}

/// The surroundings of a handler that only reads the settings.
fn env_with(cfg: &MonitorConfig) -> Env {
    let (tx, _) = unbounded_channel();
    let (panic_tx, _) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    env_from(noop_cb, cfg, &HashMap::new(), &tx, &group)
}

/// A single-account quiz with one unresolved conflict and no live countdown — the exact state
/// after a user holds a paper that still has a conflict.
fn quiz_with_conflict() -> (HashMap<ActivityKey, QuizActivity>, ActivityKey) {
    let key = ActivityKey::new("http://x", "quiz:exam", "act1");
    let attempt = PerAccountAttempt {
        state: AttemptState::Ready,
        prepare_generation: 1,
        prepare_at: Instant::now(),
        prepare_deadline: None,
        answer_contract: Some(vec![
            json!({ "id": "subj1", "type": "short_answer", "content": "Question" }),
        ]),
        instance_id: "instance-acc1".to_string(),
        subjects: vec![json!({ "id": "subj1", "type": "short_answer", "content": "Question" })],
        generated_answers: HashMap::from([("subj1".to_string(), Answer::Text("llm".to_string()))]),
        existing_answers: HashMap::from([("subj1".to_string(), Answer::Text("old".to_string()))]),
        overrides: HashMap::new(),
        conflicts: HashSet::from(["subj1".to_string()]),
        submitted_subjects: HashSet::new(),
    };
    let q = QuizActivity {
        activity_token: "token1".to_string(),
        source: Source::Exam,
        course: String::new(),
        course_id: String::new(),
        activity_id: "act1".to_string(),
        stem: String::new(),
        attempts: HashMap::from([("acc1".to_string(), attempt)]),
        countdown_deadline: None,
        held: false,
        discarded: false,
        sources: HashSet::new(),
        plan_generation: 0,
        mutation_blocked: false,
    };
    let mut quizzes = HashMap::new();
    quizzes.insert(key.clone(), q);
    (quizzes, key)
}

#[test]
fn held_quiz_does_not_rearm_countdown_when_conflict_resolves() {
    let (mut quizzes, key) = quiz_with_conflict();
    quizzes.get_mut(&key).unwrap().held = true; // user held while a conflict was still open
    let cfg = cfg_countdown(15);
    let env = env_with(&cfg);
    quiz::find_mut(&mut quizzes, "token1")
        .and_then(|quiz| {
            set_answer(
                quiz,
                &env,
                "acc1",
                "subj1",
                AnswerWire::Text { value: "x".into() },
            )
        })
        .unwrap();
    let q = quizzes.get(&key).unwrap();
    assert!(
        q.attempts["acc1"].conflicts.is_empty(),
        "the conflict is resolved"
    );
    assert!(
        q.countdown_deadline.is_none(),
        "a HELD quiz must not re-arm auto-submit — only SubmitNow may"
    );
}

#[test]
fn unheld_quiz_rearms_countdown_when_conflict_resolves() {
    let (mut quizzes, key) = quiz_with_conflict(); // held = false
    let cfg = cfg_countdown(15);
    let env = env_with(&cfg);
    quiz::find_mut(&mut quizzes, "token1")
        .and_then(|quiz| {
            set_answer(
                quiz,
                &env,
                "acc1",
                "subj1",
                AnswerWire::Text { value: "x".into() },
            )
        })
        .unwrap();
    let q = quizzes.get(&key).unwrap();
    assert!(
        q.countdown_deadline.is_some(),
        "an un-held quiz re-arms once its last conflict clears"
    );
}

#[test]
fn invalid_typed_answer_does_not_resolve_conflict() {
    let (mut quizzes, key) = quiz_with_conflict();
    let cfg = cfg_countdown(15);

    let env = env_with(&cfg);
    let result = quiz::find_mut(&mut quizzes, "token1").and_then(|quiz| {
        set_answer(
            quiz,
            &env,
            "acc1",
            "subj1",
            AnswerWire::Options {
                option_ids: vec!["not-for-text".into()],
            },
        )
    });

    assert!(result.is_err());
    assert!(quizzes[&key].attempts["acc1"].conflicts.contains("subj1"));
    assert!(quizzes[&key].attempts["acc1"].overrides.is_empty());
}

#[test]
fn late_participant_gets_own_waiting_attempt_and_cancels_countdown() {
    let (mut quizzes, key) = quiz_with_conflict();
    quizzes
        .get_mut(&key)
        .unwrap()
        .attempts
        .get_mut("acc1")
        .unwrap()
        .conflicts
        .clear();
    quizzes.get_mut(&key).unwrap().countdown_deadline = Some(Instant::now());

    quiz::on_detected(
        &mut quizzes,
        &plan_for("acc2", &["acc2"]),
        QuizDetected {
            generation: 1,
            account_id: "acc2".to_string(),
            base_url: "http://x".to_string(),
            source: Source::Exam,
            course: String::new(),
            course_id: String::new(),
            activity_id: "act1".to_string(),
            stem: String::new(),
        },
    );

    let quiz = &quizzes[&key];
    assert_eq!(quiz.attempts["acc1"].instance_id, "instance-acc1");
    assert_eq!(quiz.attempts["acc2"].state, AttemptState::Waiting);
    assert!(quiz.countdown_deadline.is_none());
}

#[test]
fn stale_prepare_generation_cannot_overwrite_new_attempt() {
    let (mut quizzes, key) = quiz_with_conflict();
    let attempt = quizzes
        .get_mut(&key)
        .unwrap()
        .attempts
        .get_mut("acc1")
        .unwrap();
    attempt.state = AttemptState::Preparing;
    attempt.prepare_generation = 2;
    let cfg = cfg_countdown(15);

    let env = env_with(&cfg);
    on_prepared(
        &mut quizzes,
        &env,
        &key,
        vec![PreparedAttempt {
            account_id: "acc1".to_string(),
            generation: 1,
            instance_id: "stale-instance".to_string(),
            subjects: vec![json!({ "id": "stale", "type": "short_answer" })],
            generated_answers: HashMap::from([(
                "stale".to_string(),
                Answer::Text("stale".to_string()),
            )]),
            existing_answers: HashMap::new(),
        }],
    );

    let attempt = &quizzes[&key].attempts["acc1"];
    assert_eq!(attempt.prepare_generation, 2);
    assert_eq!(attempt.instance_id, "instance-acc1");
    assert_eq!(attempt.state, AttemptState::Preparing);
}

#[test]
fn paper_contract_ignores_existing_answer_but_not_option_identity() {
    let alice = vec![json!({
        "id": "s1",
        "type": "single_selection",
        "options": [{ "id": "alice-o1", "content": "A" }]
    })];
    let mut same_contract = alice.clone();
    same_contract[0]["student_answer_option_ids"] = json!(["alice-o1"]);
    let different_options = vec![json!({
        "id": "s1",
        "type": "single_selection",
        "options": [{ "id": "bob-o1", "content": "A" }]
    })];

    assert_eq!(paper_contract(&alice), paper_contract(&same_contract));
    assert_ne!(paper_contract(&alice), paper_contract(&different_options));
}

#[test]
fn changed_paper_contract_drops_partial_answers() {
    let prior = PriorAnswers {
        contract: vec![json!({ "id": "s1", "options": [{ "id": "old" }] })],
        answers: HashMap::from([("s1".to_string(), Answer::Options(vec!["old".to_string()]))]),
    };
    let changed = vec![json!({ "id": "s1", "options": [{ "id": "new" }] })];

    assert!(compatible_prior(Some(&prior), &changed).is_empty());
    assert_eq!(
        compatible_prior(Some(&prior), &prior.contract),
        prior.answers
    );
}

#[test]
fn gone_attempt_does_not_block_another_ready_account() {
    let (mut quizzes, key) = quiz_with_conflict();
    let quiz = quizzes.get_mut(&key).unwrap();
    quiz.attempts.get_mut("acc1").unwrap().conflicts.clear();
    quiz.attempts.insert(
        "acc2".to_string(),
        PerAccountAttempt {
            state: AttemptState::Preparing,
            prepare_generation: 3,
            prepare_at: Instant::now(),
            prepare_deadline: None,
            answer_contract: None,
            instance_id: String::new(),
            subjects: Vec::new(),
            generated_answers: HashMap::new(),
            existing_answers: HashMap::new(),
            overrides: HashMap::new(),
            conflicts: HashSet::new(),
            submitted_subjects: HashSet::new(),
        },
    );

    on_prepare_gone(&mut quizzes, &env_with(&cfg_countdown(15)), &key, "acc2", 3);

    assert_eq!(quizzes[&key].attempts["acc2"].state, AttemptState::Gone);
    assert!(quizzes[&key].countdown_deadline.is_some());
}

#[test]
fn failed_classroom_submit_retains_completed_subjects_for_retry() {
    let (mut quizzes, key) = quiz_with_conflict();
    let attempt = quizzes
        .get_mut(&key)
        .unwrap()
        .attempts
        .get_mut("acc1")
        .unwrap();
    attempt.conflicts.clear();
    attempt.state = AttemptState::Submitting;

    on_submit_result(
        &mut quizzes,
        &env_with(&cfg_countdown(15)),
        &key,
        "acc1",
        Err(QuizSubmitFailure {
            error: "second subject failed".to_string(),
            completed_subjects: vec!["subj1".to_string()],
        }),
    );

    let attempt = &quizzes[&key].attempts["acc1"];
    assert_eq!(attempt.state, AttemptState::Ready);
    assert_eq!(
        attempt.submitted_subjects,
        HashSet::from(["subj1".to_string()])
    );
}

#[tokio::test]
async fn classroom_retry_skips_every_subject_already_confirmed_by_the_server() {
    let account = Account {
        id: "acc1".to_string(),
        device_id: String::new(),
        user_no: String::new(),
        is_teacher: false,
        course_id: None,
        base_url: "http://127.0.0.1:1".to_string(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    };
    let endpoints = Endpoints::derive(&account.base_url);
    let subjects = vec![
        json!({"id": "s1", "type": "short_answer"}),
        json!({"id": "s2", "type": "short_answer"}),
    ];
    let answers = HashMap::from([
        ("s1".to_string(), Answer::Text("a".to_string())),
        ("s2".to_string(), Answer::Text("b".to_string())),
    ]);
    let already_submitted = HashSet::from(["s1".to_string(), "s2".to_string()]);

    let completed = match submit_classroom(
        &account,
        &endpoints,
        "quiz",
        "instance",
        &subjects,
        &answers,
        &already_submitted,
    )
    .await
    {
        Ok(completed) => completed,
        Err(failure) => panic!("unexpected retry failure: {}", failure.error),
    };

    // Port 1 is deliberately unreachable: success proves the retry did not resend either
    // server-confirmed mutation.
    assert!(completed.is_empty());
}

#[test]
fn hold_is_rejected_once_submission_has_begun() {
    let (mut quizzes, key) = quiz_with_conflict();
    let q = quizzes.get_mut(&key).unwrap();
    q.attempts.get_mut("acc1").unwrap().state = AttemptState::Submitting;
    q.countdown_deadline = Some(Instant::now());

    let err = hold(q).expect_err("a begun submission must not be held");
    assert!(err.contains("submission has begun"), "unexpected: {err}");
    let q = &quizzes[&key];
    assert!(!q.held, "hold must not mutate once submission began");
    assert!(
        q.countdown_deadline.is_some(),
        "hold must not abort the in-flight mutation"
    );
}

#[test]
fn discard_is_rejected_once_submission_has_begun() {
    let (mut quizzes, key) = quiz_with_conflict();
    let q = quizzes.get_mut(&key).unwrap();
    q.attempts.get_mut("acc1").unwrap().state = AttemptState::Submitted;

    let err = discard(q, noop_cb).expect_err("a committed submission must not be discarded");
    assert!(err.contains("submission has begun"), "unexpected: {err}");
    assert!(!quizzes[&key].discarded);
}

#[test]
fn hold_and_discard_work_before_submission() {
    let (mut quizzes, key) = quiz_with_conflict();
    assert!(hold(quizzes.get_mut(&key).unwrap()).is_ok());
    assert!(quizzes[&key].held);
    assert!(discard(quizzes.get_mut(&key).unwrap(), noop_cb).is_ok());
    assert!(quizzes[&key].discarded);
}

#[test]
fn the_seen_key_separates_families_and_courses() {
    let exam = seen_key(Source::Exam, "c1", "42");
    assert_ne!(exam, seen_key(Source::CoursewareQuiz, "c1", "42"));
    assert_ne!(exam, seen_key(Source::Exam, "c2", "42"));
    assert_eq!(exam, seen_key(Source::Exam, "c1", "42"));
}

#[test]
fn an_activity_id_shared_by_two_families_is_reported_once_for_each() {
    let account = account_at("acc1", false);
    let endpoints = Endpoints::derive(&account.base_url);
    let (tx, mut rx) = unbounded_channel();
    let scan = Scan {
        account: &account,
        endpoints: &endpoints,
        tx: &tx,
        generation: 1,
    };
    let mut scanner = QuizScanner::default();
    let activity = json!({ "id": "42" });
    scanner.report(&scan, Source::Exam, "c1", &activity);
    scanner.report(&scan, Source::CoursewareQuiz, "c1", &activity);
    scanner.report(&scan, Source::Exam, "c1", &activity);

    let reported: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|message| match message {
            MonitorMsg::QuizDetected(found) => (found.source, found.course_id, found.activity_id),
            _ => panic!("unexpected message"),
        })
        .collect();
    assert_eq!(
        reported,
        [
            (Source::Exam, "c1".to_string(), "42".to_string()),
            (Source::CoursewareQuiz, "c1".to_string(), "42".to_string()),
        ]
    );
}

#[test]
fn homework_answerable_gates_on_explicit_signals_only() {
    let now = 1_700_000_000;
    // open: started, not closed, not submitted, no window fields.
    assert!(homework_answerable(
        &json!({"is_started": true, "is_closed": false}),
        now
    ));
    // explicit is_started:false → skip.
    assert!(!homework_answerable(&json!({"is_started": false}), now));
    // closed / submitted → skip.
    assert!(!homework_answerable(&json!({"is_closed": true}), now));
    assert!(!homework_answerable(&json!({"has_submitted": true}), now));
    // start/end epochs showing not-started/ended → skip.
    assert!(!homework_answerable(
        &json!({"start_time": "2099-01-01T00:00:00Z"}),
        now
    ));
    assert!(!homework_answerable(
        &json!({"end_time": "2000-01-01T00:00:00Z"}),
        now
    ));
    assert!(!homework_answerable(
        &json!({"start_time": 2_000_000_000_i64}),
        now
    ));
    // absent fields keep the open default — no guessing from missing data.
    assert!(homework_answerable(&json!({}), now));
    // a present malformed deadline fails closed (same rule as exam deadlines).
    assert!(!homework_answerable(
        &json!({"end_time": "2023-02-29T00:00:00Z"}),
        now
    ));
    // a fully open window passes.
    assert!(homework_answerable(
        &json!({"start_time": "2000-01-01T00:00:00Z", "end_time": "2099-01-01T00:00:00Z"}),
        now
    ));
}

#[test]
fn correction_failure_is_terminal_after_the_initial_exam_submit_commits() {
    let report = finalize_exam_submit("submission-1", Err("review unavailable".to_string()));
    assert_eq!(report.detail, "submitted submission-1");
    assert!(report.warning.as_deref().is_some_and(|warning| {
        warning.contains("review unavailable") && warning.contains("initial submit")
    }));
}

// --- rollcall gate / re-login / SignNow state machines (pure; no network) ---

fn detected_for(account: &str, rollcall: &str) -> Detected {
    Detected {
        generation: 1,
        account_id: account.to_string(),
        base_url: "http://x".to_string(),
        rollcall_id: rollcall.to_string(),
        kind: RollcallKind::Number,
        course: String::new(),
        course_id: None,
    }
}

fn plan_for(detector: &str, participants: &[&str]) -> MonitorPlan {
    MonitorPlan {
        generation: 1,
        routes: vec![MonitorRoute {
            source_targets: vec![TargetId::account(detector)],
            detector_account_id: detector.to_string(),
            participant_account_ids: participants
                .iter()
                .map(|participant| (*participant).to_string())
                .collect(),
            course_ids: Vec::new(),
        }],
    }
}

fn gate_activity(participants: &[&str]) -> Activity {
    Activity {
        activity_token: "token".to_string(),
        participants: participants.iter().map(|id| id.to_string()).collect(),
        ..Activity::new(RollcallKind::Number, String::new(), 1)
    }
}

#[test]
fn qr_without_teacher_stays_unacted_and_manual_retry_explains_recovery() {
    let cfg = cfg_countdown(15);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let accounts: HashMap<String, Arc<Account>> = HashMap::new();
    let key = ActivityKey::new("http://x", "qrcode", "qr1");
    let mut activity = gate_activity(&["acc1"]);
    activity.kind = RollcallKind::Qr;
    let mut activities = HashMap::from([(key.clone(), activity)]);

    let env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    dispatch_signs_for(&mut activities, &env, &key, vec!["acc1".to_string()]);
    assert!(
        !activities[&key].acted,
        "no teacher means no outbound request, so the activity must not become acted"
    );
    let error = on_sign_now(&mut activities, &env, &key).unwrap_err();
    assert!(error.contains("teacher helper") && error.contains("stop monitoring"));
}

#[test]
fn held_gate_rechecks_on_bounded_cadence_with_at_most_one_inflight() {
    let cfg = cfg_countdown(15);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let accounts: HashMap<String, Arc<Account>> = HashMap::new();
    let mut activities = HashMap::new();
    let t0 = Instant::now();

    // First detection starts the initial check (empty accounts → the spawn fails, so simulate the
    // in-flight request by setting the flag the way a successful spawn would).
    let mut env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    env.set_plan(plan_for("acc1", &["acc1"]));
    rollcall::on_detected(&mut activities, &env, detected_for("acc1", "rc1"));
    let key = ActivityKey::new("http://x", "number", "rc1");
    {
        let a = activities.get_mut(&key).unwrap();
        assert!(a.gate_pending && !a.gate_in_flight && a.gate_next_check.is_none());
        a.gate_in_flight = true; // the request is now on the wire
    }

    // Below-threshold result → hold, clear in-flight, schedule the next check on the cadence.
    on_gate(&mut activities, &env, &key, Some(5.0));
    let a = &activities[&key];
    assert!(a.gate_pending, "below threshold → still holding");
    assert!(
        !a.gate_in_flight,
        "the landed result cleared the in-flight flag"
    );
    assert!(a.gate_next_check.is_some(), "the next check is scheduled");
    assert!(a.countdown_deadline.is_none());

    // Before the deadline no new check may start, even on a tick.
    let before = t0 + GATE_RECHECK_INTERVAL - Duration::from_millis(1);
    assert!(!gate_check_due(&activities[&key], before));
    rollcall::on_tick(&mut activities, &env, before);
    assert!(
        activities[&key].gate_next_check.is_some(),
        "not due → schedule untouched"
    );
    assert!(!activities[&key].gate_in_flight);

    // Due: one check may start. The empty accounts map makes the spawn fail, so the activity is
    // paced again instead of being wedged by a stuck in-flight flag.
    rollcall::on_tick(&mut activities, &env, t0 + GATE_RECHECK_INTERVAL);
    assert!(!activities[&key].gate_in_flight);
    assert!(activities[&key].gate_next_check.is_some());

    // A live in-flight request at the due instant blocks a second one (one-in-flight invariant).
    {
        let a = activities.get_mut(&key).unwrap();
        a.gate_in_flight = true;
        a.gate_next_check = None;
    }
    rollcall::on_tick(&mut activities, &env, t0 + GATE_RECHECK_INTERVAL);
    let a = &activities[&key];
    assert!(a.gate_in_flight, "the in-flight request is untouched");
    assert!(
        a.gate_next_check.is_none(),
        "no double-check while one is in flight"
    );
}

#[test]
fn stale_gate_result_after_defer_never_rearms_the_countdown() {
    let cfg = cfg_countdown(15);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let accounts: HashMap<String, Arc<Account>> = HashMap::new();
    let mut activities = HashMap::new();
    let mut env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    env.set_plan(plan_for("acc1", &["acc1"]));
    rollcall::on_detected(&mut activities, &env, detected_for("acc1", "rc1"));
    let key = ActivityKey::new("http://x", "number", "rc1");
    activities.get_mut(&key).unwrap().gate_in_flight = true;

    on_defer(&mut activities, &env, &key);
    let a = &activities[&key];
    assert!(a.countdown_deadline.is_none());
    assert!(!a.gate_pending && !a.gate_in_flight && a.gate_next_check.is_none());

    // The in-flight gate response lands AFTER the Defer — it must be completely inert.
    on_gate(&mut activities, &env, &key, Some(90.0));
    let a = &activities[&key];
    assert!(
        a.countdown_deadline.is_none(),
        "Defer is terminal: a stale gate must never re-arm the countdown"
    );
    assert!(!a.gate_pending);
    assert!(!a.gate_in_flight);
}

#[test]
fn passing_gate_arms_the_countdown_exactly_once() {
    let cfg = cfg_countdown(15);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let accounts: HashMap<String, Arc<Account>> = HashMap::new();
    let mut activities = HashMap::new();
    let mut env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    env.set_plan(plan_for("acc1", &["acc1"]));
    rollcall::on_detected(&mut activities, &env, detected_for("acc1", "rc1"));
    let key = ActivityKey::new("http://x", "number", "rc1");
    activities.get_mut(&key).unwrap().gate_in_flight = true;

    on_gate(&mut activities, &env, &key, Some(50.0));
    let armed = activities[&key].countdown_deadline;
    assert!(armed.is_some(), "at/above threshold → countdown armed");
    assert!(!activities[&key].gate_pending);

    // A duplicate/stale result (e.g. from a late duplicate spawn) must not re-arm or extend it.
    on_gate(&mut activities, &env, &key, Some(80.0));
    assert_eq!(
        activities[&key].countdown_deadline, armed,
        "deadline is set exactly once"
    );
}

#[test]
fn relogin_backoff_paces_attempts_and_gives_up_bounded() {
    let t0 = Instant::now();
    let mut backoff = HashMap::new();
    assert!(
        relogin_due(&backoff, "a", t0),
        "first session loss retries immediately"
    );
    assert_eq!(relogin_failed(&mut backoff, "a", t0), 1);
    assert!(
        !relogin_due(&backoff, "a", t0 + Duration::from_secs(4)),
        "cooling before the base delay"
    );
    assert!(
        relogin_due(&backoff, "a", t0 + Duration::from_secs(5)),
        "due after the base delay"
    );

    // Each subsequent failure doubles the wait; the cap turns the state terminal.
    let mut at = t0 + Duration::from_secs(5);
    for attempt in 2..=RELOGIN_MAX_ATTEMPTS {
        assert_eq!(relogin_failed(&mut backoff, "a", at), attempt);
        if attempt < RELOGIN_MAX_ATTEMPTS {
            let delay = RELOGIN_BASE_DELAY.as_secs() * 2_u64.pow(attempt - 1);
            assert!(!relogin_due(
                &backoff,
                "a",
                at + Duration::from_secs(delay - 1)
            ));
            assert!(relogin_due(&backoff, "a", at + Duration::from_secs(delay)));
            at += Duration::from_secs(delay);
        }
    }
    assert!(
        !relogin_due(&backoff, "a", at + Duration::from_secs(86_400)),
        "a permanent credential failure never hammers the login endpoint again"
    );
    assert_eq!(
        relogin_failed(&mut backoff, "a", at),
        u32::MAX,
        "a terminal account is never double-reported"
    );
}

#[test]
fn sign_now_after_dispatch_retries_only_bounded_failed_accounts() {
    let cfg = cfg_countdown(15);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let accounts: HashMap<String, Arc<Account>> = HashMap::new();
    let mut activities = HashMap::new();
    let key = ActivityKey::new("http://x", "number", "rc1");
    let mut a = gate_activity(&["acc1", "acc2", "acc3"]);
    a.acted = true; // already dispatched once
    a.signed.insert("acc1".to_string());
    a.sign_failed.insert("acc2".to_string());
    a.needs_resign.insert("acc3".to_string()); // owned by the relogin path, not manual retry
    activities.insert(key.clone(), a);

    assert_eq!(
        retryable_accounts(&activities[&key]),
        vec!["acc2".to_string()],
        "only the non-auth failed, unsigned account is retryable"
    );

    // Bounded: an account past MAX_RESIGN leaves the retryable set.
    activities
        .get_mut(&key)
        .unwrap()
        .resign_attempts
        .insert("acc2".to_string(), MAX_RESIGN + 1);
    assert!(retryable_accounts(&activities[&key]).is_empty());

    // Nothing retryable → SignNow is a real Err, never a fake ok.
    let env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    let result = on_sign_now(&mut activities, &env, &key);
    assert!(result.is_err());
    assert!(
        activities[&key].acted,
        "acted is never cleared — no full re-dispatch / double sign"
    );
}

#[tokio::test]
async fn quiz_prepare_budget_timeout_sends_retry_not_stuck_preparing() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // School server: exam qualification (best-effort) + distribute with one pending subject.
    let school_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let school_addr = school_listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((mut stream, _)) = school_listener.accept().await else {
                break;
            };
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            let request_line = String::from_utf8_lossy(&request);
            let body = if request_line.contains("/distribute") {
                r#"{"exam_paper_instance_id": 1001, "subjects": [{"id": "s1", "type": "short_answer", "description": "q?"}]}"#
            } else {
                "{}"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    // LLM endpoint: accepts the POST, then NEVER responds — the deadline must cut it off.
    let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let llm_addr = llm_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = llm_listener.accept().await.unwrap();
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request).await;
        tokio::time::sleep(Duration::from_secs(10)).await; // silence — no bytes ever
    });

    let base_url = format!("http://{school_addr}");
    let account = Arc::new(Account {
        id: "acc1".to_string(),
        device_id: String::new(),
        user_no: "u".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: base_url.clone(),
        client: crate::http::client(),
        username: "user".to_string(),
        password: crate::store::secrets::Secret::default(),
    });
    let (tx, mut rx) = unbounded_channel::<MonitorMsg>();
    let key = ActivityKey::new(base_url, "quiz:exam", "act1");
    let mut generations = HashMap::new();
    generations.insert("acc1".to_string(), 1_u64);
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let env = env_from(noop_cb, &cfg_countdown(15), &HashMap::new(), &tx, &group);
    spawn_prepare(
        &env,
        PrepareJob {
            participants: vec![account],
            source: Source::Exam,
            activity_id: "act1".to_string(),
            activity_token: "tok".to_string(),
            course_id: "c1".to_string(),
            stem: String::new(),
            llm: LlmConfig {
                endpoint: format!("http://{llm_addr}/v1/chat/completions"),
                model: "m".to_string(),
                api_key: "k".to_string(),
                max_tokens: 0,
                enable_tools: false,
                max_tool_iterations: 0,
            },
            max_reask: 0,
            // One second for the whole round, far below the LLM client's read-idle timeout.
            prepare_budget_secs: 1,
            priors: HashMap::new(),
            generations,
            reusable: Vec::new(),
            key: key.clone(),
        },
    );
    let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a deadline-capped prepare must report within the budget")
        .expect("a message must arrive");
    match message {
        MonitorMsg::QuizPrepareRetry(PrepareRetry {
            key: got_key,
            account_id,
            generation,
            partial,
            missing,
            ..
        }) => {
            assert_eq!(got_key, key, "the retry carries the activity key");
            assert_eq!(account_id, "acc1");
            assert_eq!(generation, 1);
            assert!(
                partial.is_empty(),
                "nothing was answered before the cut-off"
            );
            assert_eq!(
                missing,
                vec!["s1".to_string()],
                "the pending subject is reported missing"
            );
        }
        _ => panic!("expected QuizPrepareRetry on budget timeout, got a different message"),
    }
    // The silent LLM must NOT produce a second message. The task may close its last sender, so
    // either channel closure or an idle timeout proves there was no follow-up event.
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv()).await,
            Ok(Some(_))
        ),
        "no follow-up message — the attempt is handed back to the actor's deadline path"
    );
}

// --- TaskGroup panic containment / cancellation (deterministic; no network) ---

#[tokio::test]
async fn task_group_panic_reports_core_panicked_once_and_pings() {
    let (panic_tx, mut panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(record_cb, panic_tx));
    group.spawn(async { panic!("boom: helper died") });

    tokio::time::timeout(Duration::from_secs(5), panic_rx.recv())
        .await
        .expect("a panicking helper must ping the watchdog")
        .expect("a ping must arrive");

    let events = take_events();
    assert_eq!(events.len(), 1, "exactly one panic event: {events:?}");
    assert_eq!(events[0]["event"].as_str(), Some("Error"));
    assert_eq!(events[0]["code"].as_str(), Some("core_panicked"));

    // One helper, one panic, one ping — never a duplicate report.
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(200), panic_rx.recv()).await,
            Ok(Some(_))
        ),
        "a single helper panic must ping exactly once"
    );
}

#[tokio::test]
async fn task_group_normal_completion_and_cancel_report_no_panic() {
    let (panic_tx, mut panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(record_cb, panic_tx));

    // Normal completion → no ping, no event.
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    group.spawn(async move {
        let _ = done_tx.send(());
    });
    let _ = tokio::time::timeout(Duration::from_secs(5), done_rx)
        .await
        .expect("the helper must complete");

    // Cancel → the child is aborted and nothing is reported (cancellation is not a panic).
    let started = Arc::new(AtomicBool::new(false));
    let ran_after_cancel = Arc::new(AtomicBool::new(false));
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
    let (s, r) = (started.clone(), ran_after_cancel.clone());
    group.spawn(async move {
        s.store(true, Ordering::SeqCst);
        let _ = start_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            r.store(true, Ordering::SeqCst);
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
        .await
        .expect("the looping helper must start");
    group.cancel();
    tokio::time::sleep(Duration::from_millis(5)).await;

    assert!(
        !ran_after_cancel.load(Ordering::SeqCst),
        "cancel must abort the helper at its next await point"
    );
    assert!(
        take_events().is_empty(),
        "normal completion and cancellation must not emit core_panicked"
    );
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(200), panic_rx.recv()).await,
            Ok(Some(_))
        ),
        "normal completion and cancellation must not ping the watchdog"
    );
}

#[tokio::test]
async fn task_group_drop_aborts_children() {
    // `start` may unwind after some pollers were spawned but before the handle is returned; the
    // group's final Arc drop must abort every tracked child — no detached tasks may survive it.
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let started = Arc::new(AtomicBool::new(false));
    let ran_after_drop = Arc::new(AtomicBool::new(false));
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
    let (s, r) = (started.clone(), ran_after_drop.clone());
    group.spawn(async move {
        s.store(true, Ordering::SeqCst);
        let _ = start_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            r.store(true, Ordering::SeqCst);
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
        .await
        .expect("the child must start");
    assert!(started.load(Ordering::SeqCst));

    drop(group); // last Arc → TaskGroup::drop

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(
        !ran_after_drop.load(Ordering::SeqCst),
        "the child must not run after the group drop"
    );
}

#[tokio::test]
async fn task_group_spawn_after_cancel_aborts_immediately() {
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    group.cancel();
    let started = Arc::new(AtomicBool::new(false));
    let s = started.clone();
    group.spawn(async move {
        s.store(true, Ordering::SeqCst);
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(
        !started.load(Ordering::SeqCst),
        "a spawn racing a completed stop must never run"
    );
}

#[tokio::test]
async fn startup_guard_armed_cancels_children_even_with_live_strong_clone() {
    // The start-unwind hole: the actor future holds its own group clone (and the group's tasks
    // hold the actor wrapper — a cycle), so the group's OWN Drop cannot run while that clone
    // lives. The armed guard must cancel the group directly, aborting every child anyway.
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let strong = group.clone(); // the actor-style reference that outlives `start`'s frame
    let started = Arc::new(AtomicBool::new(false));
    let ran_after_drop = Arc::new(AtomicBool::new(false));
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
    let (s, r) = (started.clone(), ran_after_drop.clone());
    group.spawn(async move {
        s.store(true, Ordering::SeqCst);
        let _ = start_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            r.store(true, Ordering::SeqCst);
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
        .await
        .expect("the child must start");
    assert!(started.load(Ordering::SeqCst));

    let guard = StartupGuard::new(group); // armed, as if start unwound before returning
    drop(guard);

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(
        !ran_after_drop.load(Ordering::SeqCst),
        "an armed guard must abort every child even while a strong clone keeps the group alive"
    );
    drop(strong);
}

#[tokio::test]
async fn startup_guard_disarmed_does_not_cancel() {
    // The normal start path disarms the guard just before returning the handle; dropping it
    // must be a no-op, otherwise every successful start would cancel its own monitor. The child
    // only starts AFTER the guard is dropped, so any stray cancel would abort it before it runs.
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let mut guard = StartupGuard::new(group.clone());
    let alive = Arc::new(AtomicBool::new(false));
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let a = alive.clone();
    group.spawn(async move {
        let _ = go_rx.await;
        a.store(true, Ordering::SeqCst);
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    guard.disarm();
    drop(guard);
    let _ = go_tx.send(());

    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        alive.load(Ordering::SeqCst),
        "a disarmed guard must not cancel the group"
    );
    // The child would still be looping — clean it up like a real stop would.
    group.cancel();
}

// --- late participant / QR restore (state machines; no network) ---

fn account_at(id: &str, is_teacher: bool) -> Arc<Account> {
    Arc::new(Account {
        id: id.to_string(),
        device_id: String::new(),
        user_no: String::new(),
        is_teacher,
        course_id: None,
        base_url: "http://127.0.0.1:1".to_string(), // port 1: connection refused → fails fast
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    })
}

#[tokio::test]
async fn late_participant_after_acted_is_dispatched_alone_not_full_redispatch() {
    let cfg = cfg_countdown(15);
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let mut accounts: HashMap<String, Arc<Account>> = HashMap::new();
    accounts.insert("acc1".to_string(), account_at("acc1", false));
    accounts.insert("acc2".to_string(), account_at("acc2", false));
    let key = ActivityKey::new("http://127.0.0.1:1", "number", "rc1");
    let mut activity = gate_activity(&["acc1"]);
    activity.acted = true; // already dispatched
    activity.signed.insert("acc1".to_string());
    let mut activities = HashMap::from([(key.clone(), activity)]);

    // acc2 detects the SAME already-acted rollcall after the dispatch.
    let mut env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    env.set_plan(plan_for("acc2", &["acc2"]));
    rollcall::on_detected(
        &mut activities,
        &env,
        Detected {
            generation: 1,
            account_id: "acc2".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            rollcall_id: "rc1".to_string(),
            kind: RollcallKind::Number,
            course: String::new(),
            course_id: None,
        },
    );

    // Exactly ONE dispatch: acc2's own sign (the unreachable host fails it fast). acc1, already
    // signed, must never be re-signed — a full re-dispatch would have produced two messages.
    let message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the late participant's sign must be dispatched")
        .expect("a message must arrive");
    match message {
        MonitorMsg::SignResult {
            key: got_key,
            account_id,
            ..
        } => {
            assert_eq!(got_key, key);
            assert_eq!(account_id, "acc2");
        }
        _ => panic!("expected a SignResult for the late participant"),
    }
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv()).await,
            Ok(Some(_))
        ),
        "no second dispatch — an already-signed account must never be re-signed"
    );
    let a = &activities[&key];
    assert!(a.acted, "the activity stays acted");
    assert!(a.signed.contains("acc1"));
    assert!(a.participants.contains("acc2"));
}

#[tokio::test]
async fn redispatch_qr_routes_through_teacher_assist_never_unsupported_spawn() {
    let cfg = cfg_countdown(15);
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let mut accounts: HashMap<String, Arc<Account>> = HashMap::new();
    accounts.insert("acc1".to_string(), account_at("acc1", false));
    accounts.insert("teacher1".to_string(), account_at("teacher1", true));
    let key = ActivityKey::new("http://127.0.0.1:1", "qrcode", "qr1");
    let mut activity = gate_activity(&["acc1"]);
    activity.kind = RollcallKind::Qr;
    activity.acted = true;
    activity.needs_resign.insert("acc1".to_string());
    let mut activities = HashMap::from([(key.clone(), activity)]);

    let env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    redispatch_signs(&mut activities, &env, "acc1");

    // The teacher-assist flow owns the restore: it tries (and fails) against the unreachable
    // host and reports a REAL qr error — never spawn_sign's generic "unsupported here".
    let mut message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a teacher-assist result must arrive")
        .expect("a message must arrive");
    while matches!(
        message,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match message {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        message = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("await QrSignResult after handshake")
            .expect("a message must arrive");
    }
    match message {
        MonitorMsg::QrSignResult {
            account_id,
            generation: _,
            result: Err(error),
            ..
        } => {
            assert_eq!(account_id, "acc1");
            assert!(
                error.contains("qr:"),
                "expected a real teacher-assist error, got: {error}"
            );
            assert!(
                !error.contains("unsupported"),
                "the generic spawn_sign path must never run for QR: {error}"
            );
        }
        _ => panic!("expected a failed teacher-assist QrSignResult"),
    }
    assert!(
        !activities[&key].needs_resign.contains("acc1"),
        "the recovered account left the pending set"
    );
}

#[test]
fn redispatch_qr_without_teacher_stays_pending_and_never_fake_dispatches() {
    let cfg = cfg_countdown(15);
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let accounts: HashMap<String, Arc<Account>> = HashMap::new();
    let key = ActivityKey::new("http://127.0.0.1:1", "qrcode", "qr1");
    let mut activity = gate_activity(&["acc1"]);
    activity.kind = RollcallKind::Qr;
    activity.acted = true;
    activity.needs_resign.insert("acc1".to_string());
    let mut activities = HashMap::from([(key.clone(), activity)]);

    let env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    redispatch_signs(&mut activities, &env, "acc1");

    // 0 偽派發: no teacher → nothing dispatchable — no SignResult (not even an "unsupported"
    // one) may be produced, and the account stays pending for a later restore.
    assert!(activities[&key].needs_resign.contains("acc1"));
    assert!(
        rx.try_recv().is_err(),
        "no message may be produced without a teacher"
    );
}

fn cfg_qr_remote_configured(secs: u64) -> MonitorConfig {
    MonitorConfig {
        countdown_secs: secs,
        gate_percent: 15.0,
        llm_endpoint: String::new(),
        llm_model: String::new(),
        llm_key: None,
        llm_max_tokens: 0,
        max_answer_reask: 0,
        prepare_retry_budget_secs: 0,
        autoanswer_types: vec![],
        enable_llm_tools: false,
        max_tool_iterations: 0,
        resubmit_for_correct: false,
        radar_strategy: vec![],
        number_cooldown_ms: 0,
        number_max_cooldowns: 0,
        poll_idle_secs: 5,
        quiz_detect_secs: 45,
        qr_remote_enabled: true,
        qr_remote_base_url: "https://api.hlp.qzz.io".to_string(),
        qr_remote_key: Some(Arc::new(Secret::new("k-qr".to_string()))),
    }
}

#[tokio::test]
async fn manual_qr_no_teacher_remote_configured_reaches_dispatch_not_requires_teacher() {
    let cfg = cfg_qr_remote_configured(15);
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let mut accounts: HashMap<String, Arc<Account>> = HashMap::new();
    accounts.insert("acc1".to_string(), account_at("acc1", false));
    let key = ActivityKey::new("http://127.0.0.1:1", "qrcode", "qr1");
    let mut activity = gate_activity(&["acc1"]);
    activity.kind = RollcallKind::Qr;
    let mut activities = HashMap::from([(key.clone(), activity)]);

    let env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    let result = on_sign_now(&mut activities, &env, &key);
    assert!(
        result.is_ok(),
        "remote-configured QR without teacher must reach dispatch, not requires-teacher, got: {result:?}"
    );
    assert!(
        activities[&key].acted,
        "remote dispatch must mark acted even before the async result lands"
    );
    let mut msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("remote dispatch must produce a QrSignResult path")
        .expect("a message must arrive");
    // run_qr_remote now emits QrSourceFailed (handshake) before QrSignResult when no token
    // is available. Direct-run tests have no actor draining it; skip it and await the real result.
    if matches!(msg, MonitorMsg::QrSourceFailed { .. }) {
        if let MonitorMsg::QrSourceFailed { ack, .. } = msg {
            let _ = ack.send(());
        }
        msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("await QrSignResult after QrSourceFailed")
            .expect("a message must arrive");
    }
    match msg {
        MonitorMsg::QrSignResult { account_id, .. } => assert_eq!(account_id, "acc1"),
        _ => panic!("expected QrSignResult"),
    }
}

#[test]
fn manual_qr_disabled_or_missing_key_still_requires_teacher() {
    let (tx, _rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let accounts: HashMap<String, Arc<Account>> = HashMap::new();
    let key = ActivityKey::new("http://127.0.0.1:1", "qrcode", "qr1");
    for cfg in [
        cfg_countdown(15),
        {
            let mut c = cfg_qr_remote_configured(15);
            c.qr_remote_enabled = false;
            c
        },
        {
            let mut c = cfg_qr_remote_configured(15);
            c.qr_remote_key = None;
            c
        },
        {
            let mut c = cfg_qr_remote_configured(15);
            c.qr_remote_key = Some(Arc::new(Secret::new("   ".to_string())));
            c
        },
    ] {
        let mut activity = gate_activity(&["acc1"]);
        activity.kind = RollcallKind::Qr;
        let mut activities = HashMap::from([(key.clone(), activity)]);
        let env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
        let err = on_sign_now(&mut activities, &env, &key).unwrap_err();
        assert!(
            err.contains("teacher helper"),
            "disabled/missing-key QR must retain teacher-required behavior, got: {err}"
        );
        assert!(!activities[&key].acted, "rejected QR must remain unacted");
    }
}

#[tokio::test]
async fn late_qr_no_teacher_remote_configured_is_dispatched_exactly_once() {
    let cfg = cfg_qr_remote_configured(15);
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let mut accounts: HashMap<String, Arc<Account>> = HashMap::new();
    accounts.insert("acc1".to_string(), account_at("acc1", false));
    accounts.insert("acc2".to_string(), account_at("acc2", false));
    let key = ActivityKey::new("http://127.0.0.1:1", "qrcode", "qr1");
    let mut activity = gate_activity(&["acc1"]);
    activity.kind = RollcallKind::Qr;
    activity.acted = true;
    activity.signed.insert("acc1".to_string());
    let mut activities = HashMap::from([(key.clone(), activity)]);

    let mut env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    env.set_plan(plan_for("acc2", &["acc2"]));
    rollcall::on_detected(
        &mut activities,
        &env,
        Detected {
            generation: 1,
            account_id: "acc2".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            rollcall_id: "qr1".to_string(),
            kind: RollcallKind::Qr,
            course: String::new(),
            course_id: None,
        },
    );

    let mut msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("remote-configured late QR must be dispatched")
        .expect("a message must arrive");
    if matches!(msg, MonitorMsg::QrSourceFailed { .. }) {
        if let MonitorMsg::QrSourceFailed { ack, .. } = msg {
            let _ = ack.send(());
        }
        msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("await QrSignResult after QrSourceFailed")
            .expect("a message must arrive");
    }
    match msg {
        MonitorMsg::QrSignResult { account_id, .. } => assert_eq!(account_id, "acc2"),
        _ => panic!("expected QrSignResult for eligible late QR"),
    }
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv()).await,
            Ok(Some(_))
        ),
        "eligible late QR must be dispatched exactly once"
    );
}

#[test]
fn late_qr_no_source_stays_undispatched_and_redispatch_stays_pending() {
    let cfg = cfg_countdown(15);
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let mut accounts: HashMap<String, Arc<Account>> = HashMap::new();
    accounts.insert("acc1".to_string(), account_at("acc1", false));
    accounts.insert("acc2".to_string(), account_at("acc2", false));
    let key = ActivityKey::new("http://127.0.0.1:1", "qrcode", "qr1");
    let mut activity = gate_activity(&["acc1"]);
    activity.kind = RollcallKind::Qr;
    activity.acted = true;
    activity.signed.insert("acc1".to_string());
    let mut activities = HashMap::from([(key.clone(), activity)]);

    let mut env = env_from(noop_cb, &cfg, &accounts, &tx, &group);
    env.set_plan(plan_for("acc2", &["acc2"]));
    rollcall::on_detected(
        &mut activities,
        &env,
        Detected {
            generation: 1,
            account_id: "acc2".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            rollcall_id: "qr1".to_string(),
            kind: RollcallKind::Qr,
            course: String::new(),
            course_id: None,
        },
    );
    assert!(
        rx.try_recv().is_err(),
        "no source → late QR must not dispatch"
    );
    assert!(activities[&key].participants.contains("acc2"));

    // Also: redispatch with no source must stay pending (no fake dispatch).
    let key2 = ActivityKey::new("http://127.0.0.1:1", "qrcode", "qr2");
    let mut activity2 = gate_activity(&["acc1"]);
    activity2.kind = RollcallKind::Qr;
    activity2.acted = true;
    activity2.needs_resign.insert("acc1".to_string());
    let mut activities2 = HashMap::from([(key2.clone(), activity2)]);
    redispatch_signs(&mut activities2, &env, "acc1");
    assert!(activities2[&key2].needs_resign.contains("acc1"));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn monitor_handle_drop_cancels_group_children() {
    // A discarded handle (re-Init replacing CoreState, teardown, error unwinds) must stop the
    // monitor: the actor ↔ group cycle keeps the group alive, so only the handle's Drop (or an
    // explicit stop) can abort the tracked tasks.
    let (tx, _rx) = unbounded_channel();
    let handle = MonitorHandle::new(tx);
    let started = Arc::new(AtomicBool::new(false));
    let ran_after_drop = Arc::new(AtomicBool::new(false));
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
    let (s, r) = (started.clone(), ran_after_drop.clone());
    handle.group.spawn(async move {
        s.store(true, Ordering::SeqCst);
        let _ = start_tx.send(());
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            r.store(true, Ordering::SeqCst);
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(5), start_rx)
        .await
        .expect("the child must start");
    assert!(started.load(Ordering::SeqCst));

    drop(handle);

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(
        !ran_after_drop.load(Ordering::SeqCst),
        "dropping the handle must cancel every tracked child"
    );
}
#[test]
fn overlapping_group_routes_fan_out_once_and_union_sources() {
    let cfg = cfg_countdown(15);
    let (tx, _rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let plan = MonitorPlan {
        generation: 9,
        routes: vec![
            MonitorRoute {
                source_targets: vec![TargetId::group("g1")],
                detector_account_id: "detector".into(),
                participant_account_ids: vec!["a".into(), "b".into()],
                course_ids: Vec::new(),
            },
            MonitorRoute {
                source_targets: vec![TargetId::group("g2")],
                detector_account_id: "detector".into(),
                participant_account_ids: vec!["b".into(), "c".into()],
                course_ids: Vec::new(),
            },
        ],
    };
    let detection = Detected {
        generation: 9,
        account_id: "detector".into(),
        base_url: "https://example.test".into(),
        rollcall_id: "rollcall".into(),
        kind: RollcallKind::Number,
        course: "共同課程".into(),
        course_id: Some("course".into()),
    };
    let mut activities = HashMap::new();
    let mut env = env_from(noop_cb, &cfg, &HashMap::new(), &tx, &group);
    env.set_plan(plan);
    rollcall::on_detected(&mut activities, &env, detection);
    let activity = activities.values().next().unwrap();
    assert_eq!(
        activity.participants,
        HashSet::from(["a".into(), "b".into(), "c".into()])
    );
    assert_eq!(
        activity.sources,
        HashSet::from([TargetId::group("g1"), TargetId::group("g2")])
    );
}

#[test]
fn removing_last_source_cancels_pending_but_retains_authorized_execution() {
    let mut pending = gate_activity(&["a"]);
    pending.sources.insert(TargetId::account("a"));
    let mut authorized = gate_activity(&["b"]);
    authorized.sources.insert(TargetId::account("b"));
    authorized.acted = true;
    let mut activities = HashMap::from([
        (ActivityKey::new("x", "number", "pending"), pending),
        (ActivityKey::new("x", "number", "authorized"), authorized),
    ]);
    drop_removed_sources(
        &mut activities,
        &mut HashMap::new(),
        &MonitorPlan {
            generation: 2,
            routes: Vec::new(),
        },
    );
    assert!(!activities.keys().any(|key| key.id == "pending"));
    assert!(activities.keys().any(|key| key.id == "authorized"));
}

#[test]
fn definition_barrier_denies_mutation_until_rollback() {
    let cfg = cfg_countdown(15);
    let (tx, _rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let key = ActivityKey::new("x", "number", "rollcall");
    let mut activity = gate_activity(&["a"]);
    activity.sources.insert(TargetId::account("a"));
    activity.mutation_blocked = true;
    let mut activities = HashMap::from([(key.clone(), activity)]);
    let env = env_from(noop_cb, &cfg, &HashMap::new(), &tx, &group);
    dispatch_signs(&mut activities, &env, &key);
    assert!(
        !activities[&key].acted,
        "barrier must deny the mutation permit"
    );
    activities.get_mut(&key).unwrap().mutation_blocked = false;
    dispatch_signs(&mut activities, &env, &key);
    assert!(
        activities[&key].acted,
        "rollback re-enables the pending permit"
    );
}

// --- teacher → remote fallback: single absolute deadline, before-mutation only ---

fn valid_qr_token() -> String {
    format!("{}{}", "0123456789", "a".repeat(32))
}

#[tokio::test]
async fn qr_teacher_prepare_failure_before_put_fallbacks_to_remote_with_one_put_max() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let token = valid_qr_token();
    let remote_hits = Arc::new(AtomicUsize::new(0));
    let student_puts = Arc::new(AtomicUsize::new(0));

    // teacher: always fail create (Transient) -> before any PUT
    let teacher_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let teacher_addr = teacher_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = teacher_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("POST /api/course/") && text.contains("/rollcall") {
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else if text.contains("POST /login") || text.contains("POST /sso/authenticate") {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nConnection: close\r\n\r\nlogin".to_string()
                } else {
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    // remote: serve valid token
    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let rh = remote_hits.clone();
    let tkn = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let rh = rh.clone();
            let tkn = tkn.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("/token") {
                    rh.fetch_add(1, Ordering::SeqCst);
                    let body = format!(r#"{{"ok":true,"data":"{tkn}"}}"#);
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    // student: count PUT, confirm recheck
    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/")
                    && text.contains("answer_qr_rollcall")
                {
                    sp.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string()
                } else if text.contains("GET /api/rollcall/") && text.contains("student_rollcalls")
                {
                    let body = r#"{"status":"on_call_fine","student_rollcalls":[{"user_no":"u1","rollcall_status":"on_call_fine"}]}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let teacher_base = format!("http://{teacher_addr}");
    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let teacher = Arc::new(Account {
        id: "teacher1".to_string(),
        device_id: String::new(),
        user_no: String::new(),
        is_teacher: true,
        course_id: Some("C1".to_string()),
        base_url: teacher_base,
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base, "qrcode", "RC-PREPARE");
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    spawn_qr_teacher_assist(
        teacher,
        vec![student],
        tx,
        key.clone(),
        1,
        leaked_plan_rx(),
        &group,
        Some((remote_base, secret, leaked_qr_rev_rx(), 0)),
    );
    let mut msg = tokio::time::timeout(Duration::from_secs(8), rx.recv())
        .await
        .expect("fallback must produce a result within window")
        .expect("a message must arrive");
    while matches!(
        msg,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        msg = tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .expect("await QrSignResult after handshake")
            .expect("a message must arrive");
    }
    match msg {
        MonitorMsg::QrSignResult {
            account_id, result, ..
        } => {
            assert_eq!(account_id, "s1");
            assert!(result.is_ok(), "fallback sign must succeed");
        }
        _ => panic!("expected QrSignResult"),
    }
    assert!(
        remote_hits.load(Ordering::SeqCst) >= 1,
        "remote token must have been requested"
    );
    assert!(
        student_puts.load(Ordering::SeqCst) <= 1,
        "at most one student PUT"
    );
    assert!(
        remote_hits.load(Ordering::SeqCst) <= 4,
        "remote retries bounded"
    );
}

#[tokio::test]
async fn qr_teacher_first_fetch_failure_before_put_fallbacks_to_remote() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let token = valid_qr_token();
    let remote_hits = Arc::new(AtomicUsize::new(0));
    let student_puts = Arc::new(AtomicUsize::new(0));
    let stop_hits = Arc::new(AtomicUsize::new(0));

    // teacher: create+start ok, first fetch fails with 500, stop ok
    let teacher_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let teacher_addr = teacher_listener.local_addr().unwrap();
    let sh = stop_hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = teacher_listener.accept().await else {
                break;
            };
            let sh = sh.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("POST /api/course/C1/rollcall") {
                    let body = r#"{"id":"t-qr-1"}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else if text.contains("POST /api/rollcall/t-qr-1/start-rollcall") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else if text.contains("GET /api/course/C1/rollcall/t-qr-1/qr_code") {
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else if text.contains("PUT /api/rollcall/t-qr-1/stop_qr_rollcall") {
                    sh.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let rh = remote_hits.clone();
    let tkn = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let rh = rh.clone();
            let tkn = tkn.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("/token") {
                    rh.fetch_add(1, Ordering::SeqCst);
                    let body = format!(r#"{{"ok":true,"data":"{tkn}"}}"#);
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/")
                    && text.contains("answer_qr_rollcall")
                {
                    sp.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                        .to_string()
                } else if text.contains("GET /api/rollcall/") && text.contains("student_rollcalls")
                {
                    let body = r#"{"status":"on_call_fine","student_rollcalls":[{"user_no":"u1","rollcall_status":"on_call_fine"}]}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let teacher_base = format!("http://{teacher_addr}");
    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let teacher = Arc::new(Account {
        id: "teacher1".to_string(),
        device_id: String::new(),
        user_no: String::new(),
        is_teacher: true,
        course_id: Some("C1".to_string()),
        base_url: teacher_base,
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base, "qrcode", "RC-FETCH");
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    spawn_qr_teacher_assist(
        teacher,
        vec![student],
        tx,
        key.clone(),
        1,
        leaked_plan_rx(),
        &group,
        Some((remote_base, secret, leaked_qr_rev_rx(), 0)),
    );
    let mut msg = tokio::time::timeout(Duration::from_secs(8), rx.recv())
        .await
        .expect("fetch failure fallback must produce result")
        .expect("a message must arrive");
    while matches!(
        msg,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        msg = tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .expect("await QrSignResult after handshake")
            .expect("a message must arrive");
    }
    match msg {
        MonitorMsg::QrSignResult {
            account_id, result, ..
        } => {
            assert_eq!(account_id, "s1");
            assert!(result.is_ok(), "fallback after fetch failure must succeed");
        }
        _ => panic!("expected QrSignResult"),
    }
    assert!(
        remote_hits.load(Ordering::SeqCst) >= 1,
        "remote must have been used after first fetch failure"
    );
    assert!(
        student_puts.load(Ordering::SeqCst) <= 1,
        "at most one PUT after fallback"
    );
    assert!(
        stop_hits.load(Ordering::SeqCst) >= 1,
        "teacher source must be cleaned up before fallback"
    );
}

#[tokio::test]
async fn qr_teacher_fanout_started_never_fallbacks_even_when_student_fails() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let remote_hits = Arc::new(AtomicUsize::new(0));
    let student_puts = Arc::new(AtomicUsize::new(0));

    // teacher: create+start ok, fetch returns valid data
    let teacher_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let teacher_addr = teacher_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = teacher_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("POST /api/course/C1/rollcall") {
                    let body = r#"{"id":"t-qr-2"}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else if text.contains("POST /api/rollcall/t-qr-2/start-rollcall") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else if text.contains("GET /api/course/C1/rollcall/t-qr-2/qr_code") {
                    let body = r#"{"data":"QRDATA-XYZ"}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else if text.contains("PUT /api/rollcall/t-qr-2/stop_qr_rollcall") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    // remote: should receive zero hits
    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let rh = remote_hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let rh = rh.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    rh.fetch_add(1, Ordering::SeqCst);
                }
                let body = r#"{"ok":true,"data":"0123456789aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    // student: PUT fails, recheck also fails (not present)
    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/")
                    && text.contains("answer_qr_rollcall")
                {
                    sp.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else if text.contains("GET /api/rollcall/") && text.contains("student_rollcalls")
                {
                    let body = r#"{"status":"in_progress","student_rollcalls":[{"user_no":"u1","rollcall_status":"in_progress"}]}"#;
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let teacher_base = format!("http://{teacher_addr}");
    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, _rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let teacher = Arc::new(Account {
        id: "teacher1".to_string(),
        device_id: String::new(),
        user_no: String::new(),
        is_teacher: true,
        course_id: Some("C1".to_string()),
        base_url: teacher_base,
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base, "qrcode", "RC-FANOUT");
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    spawn_qr_teacher_assist(
        teacher,
        vec![student],
        tx,
        key.clone(),
        1,
        leaked_plan_rx(),
        &group,
        Some((remote_base, secret, leaked_qr_rev_rx(), 0)),
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        student_puts.load(Ordering::SeqCst) >= 1,
        "fanout must have started"
    );
    assert_eq!(
        remote_hits.load(Ordering::SeqCst),
        0,
        "remote must receive zero requests once fanout started even when student fails"
    );
    group.cancel();
}

#[tokio::test]
async fn qr_teacher_failure_without_remote_preserves_teacher_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let teacher_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let teacher_addr = teacher_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = teacher_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let resp =
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string();
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    // student should never be hit when teacher prepare fails and no remote
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let resp = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let teacher_base = format!("http://{teacher_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let teacher = Arc::new(Account {
        id: "teacher1".to_string(),
        device_id: String::new(),
        user_no: String::new(),
        is_teacher: true,
        course_id: Some("C1".to_string()),
        base_url: teacher_base,
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base, "qrcode", "RC-NOREMOTE");
    spawn_qr_teacher_assist(
        teacher,
        vec![student],
        tx,
        key.clone(),
        1,
        leaked_plan_rx(),
        &group,
        None,
    );
    let mut msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("teacher failure without remote must report")
        .expect("a message must arrive");
    while matches!(
        msg,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("await QrSignResult after handshake")
            .expect("a message must arrive");
    }
    match msg {
        MonitorMsg::QrSignResult {
            account_id, result, ..
        } => {
            assert_eq!(account_id, "s1");
            let err = result.expect_err("must be teacher error");
            assert!(
                err.contains("qr: teacher could not open"),
                "must preserve teacher error without remote, got: {err}"
            );
            assert!(
                !err.to_ascii_lowercase().contains("remote"),
                "must not mention remote"
            );
        }
        _ => panic!("expected QrSignResult"),
    }
}

// --- deadline enforcement: deterministic paused-time coverage ---

#[tokio::test]
async fn qr_remote_fetch_started_near_deadline_not_accepted_after() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::time::pause();
    let token = valid_qr_token();
    let remote_hits = Arc::new(AtomicUsize::new(0));
    let student_puts = Arc::new(AtomicUsize::new(0));

    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let rh = remote_hits.clone();
    let tkn = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let rh = rh.clone();
            let tkn = tkn.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    rh.fetch_add(1, Ordering::SeqCst);
                    // Slow loopback: respond only after 5s (past the 2s deadline).
                    tokio::time::sleep(Duration::from_millis(5000)).await;
                    let body = format!(r#"{{"ok":true,"data":"{tkn}"}}"#);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            });
        }
    });

    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/") {
                    sp.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                        .to_string()
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, mut rx) = unbounded_channel();
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base, "qrcode", "RC-DEADLINE-FETCH");
    let deadline = Instant::now() + Duration::from_millis(2000);
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let remote_base_owned = remote_base.clone();
    let key_owned = key.clone();
    group.spawn(async move {
        run_qr_remote(
            deadline,
            remote_base_owned,
            secret,
            vec![student],
            tx,
            key_owned,
            1,
            leaked_plan_rx(),
            leaked_qr_rev_rx(),
            0,
        )
        .await;
    });
    tokio::time::advance(Duration::from_millis(2500)).await;
    // Let abort propagate.
    tokio::task::yield_now().await;
    let mut msg = rx
        .recv()
        .await
        .expect("deadline must produce a sanitized error");
    while matches!(
        msg,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        msg = rx.recv().await.expect("await QrSignResult after handshake");
    }
    match msg {
        MonitorMsg::QrSignResult { result, .. } => {
            let err = result.expect_err("must be deadline error");
            assert!(
                err.to_ascii_lowercase().contains("deadline"),
                "must be sanitized deadline, got: {err}"
            );
        }
        _ => panic!("expected QrSignResult"),
    }
    assert!(
        rx.try_recv().is_err(),
        "must send exactly one error, no duplicate"
    );
    assert_eq!(
        student_puts.load(Ordering::SeqCst),
        0,
        "no student PUT must start after/near deadline"
    );
    group.cancel();
    tokio::time::resume();
}

#[tokio::test]
async fn qr_remote_fanout_does_not_start_put_after_deadline() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::time::pause();
    let token = valid_qr_token();
    let student_puts = Arc::new(AtomicUsize::new(0));

    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let tkn = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let tkn = tkn.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    let body = format!(r#"{{"ok":true,"data":"{tkn}"}}"#);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            });
        }
    });

    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/") {
                    sp.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n".to_string()
                } else if text.contains("GET /api/rollcall/") && text.contains("student_rollcalls")
                {
                    let body = r#"{"status":"on_call_fine","student_rollcalls":[{"user_no":"u1","rollcall_status":"on_call_fine"}]}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, mut rx) = unbounded_channel();
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    // Two students requested via run_qr_remote to exercise fanout deadline gating.
    let mk_student = |id: &str, user_no: &str| {
        Arc::new(Account {
            id: id.to_string(),
            device_id: format!("d-{id}"),
            user_no: user_no.to_string(),
            is_teacher: false,
            course_id: None,
            base_url: student_base.clone(),
            client: crate::http::client(),
            username: String::new(),
            password: crate::store::secrets::Secret::default(),
        })
    };
    let students = vec![mk_student("s1", "u1"), mk_student("s2", "u2")];
    let key = ActivityKey::new(student_base.clone(), "qrcode", "RC-DEADLINE-FANOUT");
    let deadline = Instant::now() + Duration::from_millis(1500);
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let rb = remote_base.clone();
    let key2 = key.clone();
    group.spawn(async move {
        run_qr_remote(
            deadline,
            rb,
            secret,
            students,
            tx,
            key2,
            1,
            leaked_plan_rx(),
            leaked_qr_rev_rx(),
            0,
        )
        .await;
    });
    tokio::time::advance(Duration::from_millis(2500)).await;
    tokio::task::yield_now().await;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Ok(msg) = rx.try_recv() {
        let msg = match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
                continue;
            }
            other => other,
        };
        if let MonitorMsg::QrSignResult {
            account_id, result, ..
        } = msg
        {
            assert!(
                seen.insert(account_id.clone()),
                "one PUT/student, no duplicate"
            );
            let err = result.expect_err("must be deadline");
            assert!(err.to_ascii_lowercase().contains("deadline"), "got: {err}");
        } else {
            panic!("expected QrSignResult");
        }
    }
    // At most one chunk could have started before deadline; none after.
    // Completion bounded: we already waited past deadline and got all errors.
    assert!(seen.len() <= 2, "bounded completion");
    group.cancel();
    tokio::time::resume();
}

#[tokio::test]
async fn qr_fallback_gets_only_remaining_time_not_new_window() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::time::pause();
    let token = valid_qr_token();
    let remote_fetch_started = Arc::new(AtomicUsize::new(0));

    let teacher_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let teacher_addr = teacher_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = teacher_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                // The teacher source fails only after consuming 11s of the 12s window.
                tokio::time::sleep(Duration::from_millis(11000)).await;
                let resp =
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let started = remote_fetch_started.clone();
    let tkn = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let started = started.clone();
            let tkn = tkn.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    started.fetch_add(1, Ordering::SeqCst);
                    // Never respond successfully before deadline — sleep past it.
                    tokio::time::sleep(Duration::from_millis(5000)).await;
                    let body = format!(r#"{{"ok":true,"data":"{tkn}"}}"#);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            });
        }
    });

    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let resp =
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let teacher_base = format!("http://{teacher_addr}");
    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let teacher = Arc::new(Account {
        id: "teacher1".to_string(),
        device_id: String::new(),
        user_no: String::new(),
        is_teacher: true,
        course_id: Some("C1".to_string()),
        base_url: teacher_base,
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base, "qrcode", "RC-FALLBACK-REMAINING");
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    // Paused time auto-advances: the teacher fails at ~11s, so the fallback starts with ~1s left
    // and its 5s token fetch must be cut off by the original deadline.
    let assist_started = Instant::now();
    spawn_qr_teacher_assist(
        teacher,
        vec![student],
        tx,
        key.clone(),
        1,
        leaked_plan_rx(),
        &group,
        Some((remote_base, secret, leaked_qr_rev_rx(), 0)),
    );
    let mut msg = rx
        .recv()
        .await
        .expect("fallback with remaining time must still report within deadline");
    while matches!(
        msg,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        msg = rx.recv().await.expect("await QrSignResult after handshake");
    }
    match msg {
        MonitorMsg::QrSignResult { result, .. } => {
            let err = result.expect_err("must be a deadline error, not success");
            assert!(err.to_ascii_lowercase().contains("deadline"), "got: {err}");
        }
        _ => panic!("expected QrSignResult"),
    }
    assert!(
        remote_fetch_started.load(Ordering::SeqCst) > 0,
        "fallback must have started inside the teacher window"
    );
    // Total elapsed from task start is one window: a fresh 12s for the fallback would let the
    // 5s token fetch finish at ~16s instead.
    let elapsed = assist_started.elapsed();
    assert!(
        elapsed <= teacher_qr::CONFIRM_WINDOW + Duration::from_millis(50),
        "elapsed: {elapsed:?}"
    );
    group.cancel();
    tokio::time::resume();
}

#[tokio::test]
async fn qr_total_acquire_plus_fanout_bounded_by_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::time::pause();
    let token = valid_qr_token();

    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let tkn = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let tkn = tkn.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    let body = format!(r#"{{"ok":true,"data":"{tkn}"}}"#);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            });
        }
    });

    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/") {
                    // Hang the student PUT so fanout must be cut by deadline.
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                        .to_string()
                } else if text.contains("GET /api/rollcall/") {
                    let body = r#"{"status":"on_call_fine","student_rollcalls":[{"user_no":"u1","rollcall_status":"on_call_fine"}]}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (tx, mut rx) = unbounded_channel();
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base.clone(), "qrcode", "RC-BOUNDED");
    let deadline = Instant::now() + Duration::from_millis(1200);
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let rb = remote_base.clone();
    let key2 = key.clone();
    group.spawn(async move {
        run_qr_remote(
            deadline,
            rb,
            secret,
            vec![student],
            tx,
            key2,
            1,
            leaked_plan_rx(),
            leaked_qr_rev_rx(),
            0,
        )
        .await;
    });
    tokio::time::advance(Duration::from_millis(2000)).await;
    tokio::task::yield_now().await;
    let mut msg = rx.recv().await.expect("bounded completion must report");
    while matches!(
        msg,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        msg = rx.recv().await.expect("await QrSignResult after handshake");
    }
    match msg {
        MonitorMsg::QrSignResult { result, .. } => {
            let err = result.expect_err("hanging fanout must become deadline");
            assert!(err.to_ascii_lowercase().contains("deadline"), "got: {err}");
        }
        _ => panic!("expected QrSignResult"),
    }
    // Completion bounded: recv arrived within the advanced window; deadline is already exceeded.
    assert!(
        Instant::now() >= deadline,
        "deadline must have been reached"
    );
    group.cancel();
    tokio::time::resume();
}
// --- the actor's handling of QR job messages ---

/// An actor on plan `generation` that already knows `activities`, with what it publishes to the
/// engine and what it sends itself.
fn actor_at(
    generation: u64,
    cfg: MonitorConfig,
    activities: impl IntoIterator<Item = (ActivityKey, Activity)>,
) -> (
    Actor,
    UnboundedReceiver<RuntimeEvent>,
    UnboundedReceiver<MonitorMsg>,
) {
    let (tx, rx) = unbounded_channel();
    let (runtime_tx, runtime_rx) = unbounded_channel();
    let (panic_tx, _) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let plan = MonitorPlan {
        generation,
        routes: Vec::new(),
    };
    let mut actor = Actor::new(Env::new(noop_cb, cfg, plan, tx, group, runtime_tx));
    actor.activities.extend(activities);
    (actor, runtime_rx, rx)
}

fn qr_key() -> ActivityKey {
    ActivityKey::new("http://x", "qrcode", "qr1")
}

fn qr_activity(participants: &[&str], generation: u64) -> Activity {
    Activity {
        kind: RollcallKind::Qr,
        plan_generation: generation,
        ..gate_activity(participants)
    }
}

fn qr_result(
    account_id: &str,
    generation: u64,
    mutation_attempted: bool,
    result: Result<SignOutcome, String>,
) -> MonitorMsg {
    MonitorMsg::QrSignResult {
        key: qr_key(),
        account_id: account_id.to_string(),
        generation,
        mutation_attempted,
        result,
    }
}

fn qr_signed(account_id: &str, generation: u64) -> MonitorMsg {
    let outcome = SignOutcome {
        method: "qr".to_string(),
        discovered_code: None,
    };
    qr_result(account_id, generation, true, Ok(outcome))
}

fn qr_source_failed(generation: u64, reason: &str) -> MonitorMsg {
    let (ack, _) = tokio::sync::oneshot::channel();
    MonitorMsg::QrSourceFailed {
        key: qr_key(),
        generation,
        reason: reason.to_string(),
        ack,
    }
}

fn untouched(activity: &Activity) -> bool {
    activity.signed.is_empty()
        && activity.sign_failed.is_empty()
        && activity.needs_resign.is_empty()
        && activity.resign_attempts.is_empty()
}

#[test]
fn a_qr_result_from_an_older_plan_is_dropped() {
    let (mut actor, mut runtime_rx, mut rx) =
        actor_at(2, cfg_countdown(15), [(qr_key(), qr_activity(&["s1"], 1))]);
    actor.handle(qr_signed("s1", 1));
    assert!(untouched(&actor.activities[&qr_key()]));
    assert!(
        runtime_rx.try_recv().is_err(),
        "a stale result is not published"
    );
    assert!(rx.try_recv().is_err(), "a stale result starts nothing");
}

#[test]
fn a_qr_result_for_an_activity_detected_under_an_older_plan_is_dropped() {
    let (mut actor, mut runtime_rx, _rx) =
        actor_at(2, cfg_countdown(15), [(qr_key(), qr_activity(&["s1"], 1))]);
    actor.handle(qr_signed("s1", 2));
    assert!(untouched(&actor.activities[&qr_key()]));
    assert!(runtime_rx.try_recv().is_err());
}

#[test]
fn a_qr_result_for_an_unknown_activity_is_dropped() {
    let (mut actor, mut runtime_rx, _rx) = actor_at(1, cfg_countdown(15), []);
    actor.handle(qr_signed("s1", 1));
    assert!(actor.activities.is_empty());
    assert!(runtime_rx.try_recv().is_err());
}

#[test]
fn a_qr_result_of_the_current_plan_is_applied_and_published() {
    let (mut actor, mut runtime_rx, _rx) =
        actor_at(2, cfg_countdown(15), [(qr_key(), qr_activity(&["s1"], 2))]);
    actor.handle(qr_signed("s1", 2));
    let activity = &actor.activities[&qr_key()];
    assert!(activity.signed.contains("s1"));
    assert_eq!(activity.qr, QrSource::Mutating);
    assert!(matches!(
        runtime_rx.try_recv(),
        Ok(RuntimeEvent::AccountResult {
            phase: AccountResultPhase::Succeeded,
            ..
        })
    ));
}

#[test]
fn a_source_failure_leaves_the_activity_untouched_and_retryable() {
    let unauthorized = "qr remote unauthorized";
    let (mut actor, _runtime_rx, _rx) = actor_at(
        1,
        cfg_countdown(15),
        [(qr_key(), qr_activity(&["acc1", "acc2"], 1))],
    );
    actor.handle(qr_source_failed(1, unauthorized));
    for account in ["acc1", "acc2"] {
        actor.handle(qr_result(account, 1, false, Err(unauthorized.to_string())));
    }
    let activity = &actor.activities[&qr_key()];
    assert_eq!(
        activity.qr,
        QrSource::Failed {
            reason: unauthorized.to_string()
        }
    );
    assert!(
        untouched(activity),
        "nothing was sent, so nothing is counted"
    );

    // A request that did go out replaces that state for good.
    actor.handle(qr_result("acc1", 1, true, Err("boom".to_string())));
    actor.handle(qr_source_failed(1, "late"));
    let activity = &actor.activities[&qr_key()];
    assert_eq!(activity.qr, QrSource::Mutating);
    assert!(activity.sign_failed.contains("acc1"));
    assert_eq!(activity.resign_attempts.get("acc1"), Some(&1));
}

#[test]
fn a_source_failure_is_recorded_even_when_the_results_arrive_first() {
    let (mut actor, _runtime_rx, _rx) = actor_at(
        1,
        cfg_countdown(15),
        [(qr_key(), qr_activity(&["acc1"], 1))],
    );
    actor.handle(qr_result("acc1", 1, false, Err("no token".to_string())));
    assert_eq!(actor.activities[&qr_key()].qr, QrSource::Untried);
    actor.handle(qr_source_failed(1, "no token"));
    assert!(matches!(
        actor.activities[&qr_key()].qr,
        QrSource::Failed { .. }
    ));
    assert!(untouched(&actor.activities[&qr_key()]));
}

#[tokio::test]
async fn a_newly_usable_remote_retries_only_what_failed_for_want_of_a_source() {
    let failed = || QrSource::Failed {
        reason: "qr remote: no data".to_string(),
    };
    let mut activity = qr_activity(&["acc1", "acc2", "acc3"], 1);
    activity.signed.insert("acc1".to_string());
    activity.needs_resign.insert("acc3".to_string());
    activity.sign_failed.insert("acc2".to_string());
    activity.resign_attempts.insert("acc2".to_string(), 1);
    activity.qr = failed();
    let (mut actor, _runtime_rx, _rx) = actor_at(1, cfg_countdown(15), [(qr_key(), activity)]);
    for id in ["acc1", "acc2", "acc3"] {
        actor
            .env
            .accounts
            .insert(id.to_string(), account_at(id, false));
    }
    let remote = |key: &str| {
        let mut cfg = cfg_countdown(15);
        cfg.qr_remote_enabled = true;
        cfg.qr_remote_base_url = "https://qr.example".to_string();
        cfg.qr_remote_key = Some(Arc::new(Secret::new(key.to_string())));
        MonitorMsg::ConfigUpdated(Box::new(cfg))
    };

    actor.handle(remote("k1"));
    let activity = &actor.activities[&qr_key()];
    assert_eq!(
        activity.qr,
        QrSource::Untried,
        "the failure is cleared for the retry"
    );
    assert!(
        activity.signed.contains("acc1"),
        "a signed account stays signed"
    );
    assert!(
        activity.needs_resign.contains("acc3"),
        "an account waiting on its session is not this retry's business"
    );
    assert!(!activity.sign_failed.contains("acc2"));
    assert!(!activity.resign_attempts.contains_key("acc2"));

    // The same settings again are not a change.
    actor.activities.get_mut(&qr_key()).unwrap().qr = failed();
    actor.handle(remote("k1"));
    assert_eq!(actor.activities[&qr_key()].qr, failed());

    // A rotated key is.
    actor.handle(remote("k2"));
    assert_eq!(actor.activities[&qr_key()].qr, QrSource::Untried);

    // An activity that may have sent a request is never retried.
    actor.activities.get_mut(&qr_key()).unwrap().qr = QrSource::Mutating;
    actor.handle(remote("k3"));
    assert_eq!(actor.activities[&qr_key()].qr, QrSource::Mutating);
}

#[test]
fn a_change_is_visible_to_tasks_that_subscribe_after_it() {
    // Nothing is subscribed when these changes happen: a plain watch `send` would drop them.
    let (mut actor, _runtime_rx, _rx) = actor_at(1, cfg_countdown(15), []);
    let (_, before) = actor.env.qr_revision();
    actor.handle(MonitorMsg::ConfigUpdated(Box::new(
        cfg_qr_remote_configured(15),
    )));
    actor.handle(MonitorMsg::ApplyPlan {
        plan: MonitorPlan {
            generation: 5,
            routes: Vec::new(),
        },
        cancel_removed_pending: false,
    });
    let (_, after) = actor.env.qr_revision();
    assert_ne!(
        after, before,
        "a later remote job must see the new settings revision"
    );
    assert_eq!(actor.env.plan_rx().borrow().generation, 5);
}

#[tokio::test]
async fn qr_generation_change_during_remote_fetch_prevents_put() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let student_puts = Arc::new(AtomicUsize::new(0));
    let remote_hits = Arc::new(AtomicUsize::new(0));
    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let rh = remote_hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let rh = rh.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    rh.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let body = format!(r#"{{"ok":true,"data":"{}"}}"#, valid_qr_token());
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                }
            });
        }
    });
    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("PUT /api/rollcall/") {
                    sp.fetch_add(1, Ordering::SeqCst);
                }
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
            });
        }
    });
    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let (plan_tx, plan_rx) = watch::channel(MonitorPlan {
        generation: 1,
        routes: Vec::new(),
    });
    let (tx, mut rx) = unbounded_channel();
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base.clone(), "qrcode", "RC-GEN-FETCH");
    let deadline = Instant::now() + Duration::from_secs(12);
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    group.spawn(async move {
        run_qr_remote(
            deadline,
            remote_base,
            secret,
            vec![student],
            tx,
            key,
            1,
            plan_rx,
            leaked_qr_rev_rx(),
            0,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = plan_tx.send(MonitorPlan {
        generation: 2,
        routes: Vec::new(),
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        student_puts.load(Ordering::SeqCst),
        0,
        "no student PUT after generation change during fetch"
    );
    assert!(
        rx.try_recv().is_err(),
        "cancelled fetch must not send QrSignResult"
    );
    group.cancel();
}

#[tokio::test]
async fn qr_generation_change_before_next_fanout_chunk_prevents_later_put() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let student_puts = Arc::new(AtomicUsize::new(0));
    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    let body = format!(r#"{{"ok":true,"data":"{}"}}"#, valid_qr_token());
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                }
            });
        }
    });
    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/") {
                    sp.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else if text.contains("GET /api/rollcall/") {
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 95\r\nConnection: close\r\n\r\n{\"status\":\"in_progress\",\"student_rollcalls\":[{\"user_no\":\"u0\",\"rollcall_status\":\"in_progress\"}]}"
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let mk_student = |id: usize| {
        Arc::new(Account {
            id: format!("s{id}"),
            device_id: format!("d{id}"),
            user_no: format!("u{id}"),
            is_teacher: false,
            course_id: None,
            base_url: student_base.clone(),
            client: crate::http::client(),
            username: String::new(),
            password: crate::store::secrets::Secret::default(),
        })
    };
    let students: Vec<Arc<Account>> = (0..9).map(mk_student).collect();
    let (plan_tx, plan_rx) = watch::channel(MonitorPlan {
        generation: 1,
        routes: Vec::new(),
    });
    let (tx, mut rx) = unbounded_channel();
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    let key = ActivityKey::new(student_base.clone(), "qrcode", "RC-GEN-CHUNK");
    let deadline = Instant::now() + Duration::from_secs(12);
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    group.spawn(async move {
        run_qr_remote(
            deadline,
            remote_base,
            secret,
            students,
            tx,
            key,
            1,
            plan_rx,
            leaked_qr_rev_rx(),
            0,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        student_puts.load(Ordering::SeqCst) >= 1,
        "first chunk must have started"
    );
    assert!(
        student_puts.load(Ordering::SeqCst) <= 8,
        "second chunk not yet started"
    );
    let _ = plan_tx.send(MonitorPlan {
        generation: 2,
        routes: Vec::new(),
    });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let puts = student_puts.load(Ordering::SeqCst);
    assert!(
        puts <= 8,
        "second chunk must not start after generation change, got {puts}"
    );
    assert!(puts >= 1, "first chunk was allowed");
    let mut msgs = 0;
    while let Ok(m) = rx.try_recv() {
        match m {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
                continue;
            }
            _ => {}
        }
        msgs += 1;
    }
    assert!(
        msgs <= 8,
        "no later chunk messages after generation change, got {msgs}"
    );
    group.cancel();
}

#[test]
fn qr_revision_change_covers_enabled_base_key_including_disable() {
    let base = cfg_countdown(15);
    let mut disabling = cfg_countdown(15);
    disabling.qr_remote_enabled = true;
    disabling.qr_remote_base_url = "https://qr.example".to_string();
    disabling.qr_remote_key = Some(Arc::new(Secret::new("k1".to_string())));
    // disabled -> enabled with key/base is a revision change
    assert!(is_qr_remote_revision_change(&base, &disabling));
    // enabled with same key/base -> no revision change when identical
    assert!(!is_qr_remote_revision_change(
        &disabling,
        &disabling.clone()
    ));
    // disable again is a revision change
    let mut enabled = disabling.clone();
    enabled.qr_remote_enabled = false;
    assert!(is_qr_remote_revision_change(&disabling, &enabled));
    // key rotation is a revision change
    let mut rotated = disabling.clone();
    rotated.qr_remote_key = Some(Arc::new(Secret::new("k2".to_string())));
    assert!(is_qr_remote_revision_change(&disabling, &rotated));
    // canonical base change (trailing slash is normalized away => no change)
    let mut slash = disabling.clone();
    slash.qr_remote_base_url = "https://qr.example/".to_string();
    assert!(!is_qr_remote_revision_change(&disabling, &slash));
    // different host is a revision change
    let mut other_host = disabling.clone();
    other_host.qr_remote_base_url = "https://other.example".to_string();
    assert!(is_qr_remote_revision_change(&disabling, &other_host));
    // unrelated setting (e.g. countdown) does not bump revision
    let mut unrelated = disabling.clone();
    unrelated.countdown_secs = 999;
    assert!(!is_qr_remote_revision_change(&disabling, &unrelated));
    let _ = base;
}

#[tokio::test]
async fn qr_remote_revision_change_during_fetch_cancels_old_and_prevents_put() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let student_puts = Arc::new(AtomicUsize::new(0));
    // Remote hangs on /token so the fetch stays in-flight.
    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                // never respond -> fetch stays pending until cancelled
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
        }
    });
    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/") {
                    sp.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    let remote_base = format!("http://{remote_addr}");
    let student_base = format!("http://{student_addr}");
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let (rev_tx, rev_rx) = watch::channel(0u64);
    let (plan_tx, plan_rx) = watch::channel(MonitorPlan {
        generation: 1,
        routes: Vec::new(),
    });
    let (tx, mut rx) = unbounded_channel();
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    let key = ActivityKey::new(student_base.clone(), "qrcode", "RC-REV-FETCH");
    let deadline = Instant::now() + Duration::from_secs(12);
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    let rev_rx_clone = rev_rx.clone();
    group.spawn(async move {
        run_qr_remote(
            deadline,
            remote_base,
            secret,
            vec![student],
            tx,
            key,
            1,
            plan_rx,
            rev_rx_clone,
            0,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    // Simulate ConfigUpdated disabling/rotating remote: synchronously bump revision.
    let _ = rev_tx.send(1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        student_puts.load(Ordering::SeqCst),
        0,
        "revision change during fetch must prevent any student PUT"
    );
    assert!(
        rx.try_recv().is_err(),
        "cancelled fetch must not emit QrSignResult with stale key/base"
    );
    // Verify old revision task is gone and a new revision can still succeed via fresh dispatch.
    let _ = plan_tx;
    group.cancel();
}

#[tokio::test]
async fn qr_teacher_fallback_refuses_stale_remote_lease_on_revision_change() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let token = valid_qr_token();
    let student_puts = Arc::new(AtomicUsize::new(0));
    let remote_hits = Arc::new(AtomicUsize::new(0));
    // Remote would succeed if fallback were allowed, but revision will already have bumped.
    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_addr = remote_listener.local_addr().unwrap();
    let tkn = token.clone();
    let rh = remote_hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = remote_listener.accept().await else {
                break;
            };
            let tkn = tkn.clone();
            let rh = rh.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if text.contains("/token") {
                    rh.fetch_add(1, Ordering::SeqCst);
                    let body = format!(r#"{{"ok":true,"data":"{tkn}"}}"#);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
            });
        }
    });
    let teacher_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let teacher_addr = teacher_listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = teacher_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                // Fail teacher create so fallback would be attempted.
                let resp = if text.contains("POST /api/course/") && text.contains("/rollcall") {
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    let student_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let student_addr = student_listener.local_addr().unwrap();
    let sp = student_puts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = student_listener.accept().await else {
                break;
            };
            let sp = sp.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = if text.contains("PUT /api/rollcall/") {
                    sp.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    let remote_base = format!("http://{remote_addr}");
    let teacher_base = format!("http://{teacher_addr}");
    let student_base = format!("http://{student_addr}");
    let teacher = Arc::new(Account {
        id: "teacher-1".to_string(),
        device_id: "dev-t".to_string(),
        user_no: "u-t".to_string(),
        is_teacher: true,
        course_id: None,
        base_url: teacher_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let student = Arc::new(Account {
        id: "s1".to_string(),
        device_id: "d1".to_string(),
        user_no: "u1".to_string(),
        is_teacher: false,
        course_id: None,
        base_url: student_base.clone(),
        client: crate::http::client(),
        username: String::new(),
        password: crate::store::secrets::Secret::default(),
    });
    let key = ActivityKey::new(student_base.clone(), "qrcode", "RC-REV-FALLBACK");
    let (tx, mut rx) = unbounded_channel();
    let (panic_tx, _panic_rx) = unbounded_channel();
    let group = Arc::new(TaskGroup::new(noop_cb, panic_tx));
    // Lease is stale: create rev channel at 0, then bump to 1 before teacher assist starts its fallback.
    let (rev_tx, rev_rx) = watch::channel(0u64);
    let _ = rev_tx.send(1);
    let stale_rx = rev_rx.clone();
    let secret = Arc::new(Secret::new("k-qr-test".to_string()));
    // rev captured is 0, but channel now holds 1 => fallback must refuse.
    spawn_qr_teacher_assist(
        teacher,
        vec![student],
        tx,
        key.clone(),
        1,
        leaked_plan_rx(),
        &group,
        Some((remote_base, secret, stale_rx, 0)),
    );
    let mut msg = tokio::time::timeout(Duration::from_secs(8), rx.recv())
        .await
        .expect("stale lease fallback must produce a result")
        .expect("a message must arrive");
    while matches!(
        msg,
        MonitorMsg::QrSourceFailed { .. } | MonitorMsg::QrMutationStarted { .. }
    ) {
        match msg {
            MonitorMsg::QrSourceFailed { ack, .. } | MonitorMsg::QrMutationStarted { ack, .. } => {
                let _ = ack.send(());
            }
            _ => {}
        }
        msg = tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .expect("await QrSignResult after handshake")
            .expect("a message must arrive");
    }
    match msg {
        MonitorMsg::QrSignResult { result, .. } => {
            assert!(
                result.is_err(),
                "stale lease must not produce a successful PUT"
            );
        }
        _ => panic!("expected QrSignResult"),
    }
    assert_eq!(
        student_puts.load(Ordering::SeqCst),
        0,
        "stale remote lease must not allow any student PUT"
    );
    assert_eq!(
        remote_hits.load(Ordering::SeqCst),
        0,
        "stale lease must not even fetch remote token"
    );
    group.cancel();
}
#[tokio::test]
async fn qr_remote_fanout_panic_sanitizes_unresolved_and_emits_core_panicked() {
    let (panic_tx, mut panic_rx) = tokio::sync::mpsc::unbounded_channel();
    let group = Arc::new(TaskGroup::new(record_cb, panic_tx));
    let key = ActivityKey::new("http://127.0.0.1:9", "qrcode", "RC-PANIC-REMOTE");
    let generation = 1u64;
    let students: Vec<Arc<Account>> = ["s1", "s2", "s3", "s4"]
        .iter()
        .map(|id| account_at(id, false))
        .collect();
    let (inner_tx, mut inner_rx) = tokio::sync::mpsc::unbounded_channel();
    let students_clone = students.clone();
    let key_clone = key.clone();
    group.spawn({
        let inner_tx = inner_tx.clone();
        let students_clone = students_clone.clone();
        let key_clone = key_clone.clone();
        async move {
            let mut fanout: JoinSet<(String, Result<SignOutcome, String>)> = JoinSet::new();
            let mut resolved: HashSet<String> = HashSet::new();
            let (s1, s2, s3, s4) = (
                students_clone[0].clone(),
                students_clone[1].clone(),
                students_clone[2].clone(),
                students_clone[3].clone(),
            );
            fanout.spawn(async move {
                let _ = s1.id.clone();
                panic!("injected fanout panic");
                #[allow(unreachable_code)]
                (
                    s1.id.clone(),
                    Ok(SignOutcome {
                        method: "qr".into(),
                        discovered_code: None,
                    }),
                )
            });
            for s in [s2, s3, s4] {
                fanout.spawn(async move {
                    std::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    (
                        s.id.clone(),
                        Ok(SignOutcome {
                            method: "qr".into(),
                            discovered_code: None,
                        }),
                    )
                });
            }
            while !fanout.is_empty() {
                let joined = fanout.join_next().await;
                match joined {
                    Some(Ok((account_id, result))) => {
                        if resolved.insert(account_id.clone()) {
                            let _ = inner_tx.send(MonitorMsg::QrSignResult {
                                key: key_clone.clone(),
                                account_id,
                                generation,
                                mutation_attempted: true,
                                result,
                            });
                        }
                    }
                    Some(Err(join_error)) => {
                        if join_error.is_panic() {
                            let payload = join_error.into_panic();
                            handle_qr_fanout_panic(
                                &mut fanout,
                                &mut resolved,
                                &students_clone,
                                &inner_tx,
                                &key_clone,
                                generation,
                                payload,
                            )
                            .await;
                        }
                    }
                    None => break,
                }
            }
            let _ = Instant::now();
        }
    });
    tokio::time::timeout(Duration::from_secs(5), panic_rx.recv())
        .await
        .expect("outer TaskGroup must ping on fanout panic")
        .expect("a ping must arrive");
    let events = take_events();
    assert_eq!(events.len(), 1, "exactly one core_panicked: {events:?}");
    assert_eq!(events[0]["code"].as_str(), Some("core_panicked"));
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_millis(100), panic_rx.recv()).await,
            Ok(Some(_))
        ),
        "single panic must ping exactly once"
    );
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut mutation_flags: HashMap<String, bool> = HashMap::new();
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(300), inner_rx.recv()).await {
            Ok(Some(MonitorMsg::QrSignResult {
                account_id,
                result,
                mutation_attempted,
                generation: g,
                ..
            })) => {
                assert_eq!(g, generation, "generation preserved");
                assert!(
                    seen.insert(account_id.clone(), result.clone().unwrap_err())
                        .is_none(),
                    "duplicate result for {account_id}"
                );
                mutation_flags.insert(account_id.clone(), mutation_attempted);
                let err = result.unwrap_err();
                assert_eq!(
                    err, "qr: internal worker failed",
                    "must be sanitized, got: {err}"
                );
                assert!(!err.contains("injected"), "must not leak panic payload");
            }
            Ok(Some(_)) => panic!("unexpected msg"),
            _ => break,
        }
    }
    assert_eq!(
        seen.len(),
        4,
        "all 4 unresolved students must get sanitized failure, got {seen:?}"
    );
    for (id, f) in &mutation_flags {
        assert!(
            *f,
            "mutation_attempted must be true conservatively for {id} to prevent auto retry"
        );
    }
    group.cancel();
}

#[tokio::test]
async fn qr_teacher_fanout_panic_sanitizes_unresolved_and_emits_core_panicked() {
    let (panic_tx, mut panic_rx) = tokio::sync::mpsc::unbounded_channel();
    let group = Arc::new(TaskGroup::new(record_cb, panic_tx));
    let key = ActivityKey::new("http://127.0.0.1:9", "qrcode", "RC-PANIC-TEACHER");
    let generation = 7u64;
    let students: Vec<Arc<Account>> = ["t1", "t2", "t3"]
        .iter()
        .map(|id| account_at(id, false))
        .collect();
    let (inner_tx, mut inner_rx) = tokio::sync::mpsc::unbounded_channel();
    let students_clone = students.clone();
    let key_clone = key.clone();
    group.spawn({
        let inner_tx = inner_tx.clone();
        let students_clone = students_clone.clone();
        let key_clone = key_clone.clone();
        async move {
            let mut fanout: JoinSet<(String, Result<SignOutcome, String>)> = JoinSet::new();
            let mut resolved: HashSet<String> = HashSet::new();
            #[allow(unused_variables)]
            let (a, b, c) = (
                students_clone[0].clone(),
                students_clone[1].clone(),
                students_clone[2].clone(),
            );
            let good_id = a.id.clone();
            fanout.spawn(async move {
                (
                    good_id.clone(),
                    Ok(SignOutcome {
                        method: "qr".into(),
                        discovered_code: None,
                    }),
                )
            });
            fanout.spawn(async move {
                let _ = b.id.clone();
                panic!("teacher fanout injected");
                #[allow(unreachable_code)]
                (
                    b.id.clone(),
                    Ok(SignOutcome {
                        method: "qr".into(),
                        discovered_code: None,
                    }),
                )
            });
            fanout.spawn(async move {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                (
                    c.id.clone(),
                    Ok(SignOutcome {
                        method: "qr".into(),
                        discovered_code: None,
                    }),
                )
            });
            while !fanout.is_empty() {
                let joined = fanout.join_next().await;
                match joined {
                    Some(Ok((account_id, result))) => {
                        if resolved.insert(account_id.clone()) {
                            let _ = inner_tx.send(MonitorMsg::QrSignResult {
                                key: key_clone.clone(),
                                account_id,
                                generation,
                                mutation_attempted: true,
                                result,
                            });
                        }
                    }
                    Some(Err(join_error)) => {
                        if join_error.is_panic() {
                            let payload = join_error.into_panic();
                            handle_qr_fanout_panic(
                                &mut fanout,
                                &mut resolved,
                                &students_clone,
                                &inner_tx,
                                &key_clone,
                                generation,
                                payload,
                            )
                            .await;
                        }
                    }
                    None => break,
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(5), panic_rx.recv())
        .await
        .expect("teacher fanout panic must ping")
        .expect("a ping must arrive");
    let events = take_events();
    assert_eq!(events.len(), 1, "exactly one core_panicked: {events:?}");
    assert_eq!(events[0]["code"].as_str(), Some("core_panicked"));
    let mut seen: HashMap<String, String> = HashMap::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(300), inner_rx.recv()).await {
            Ok(Some(MonitorMsg::QrSignResult {
                account_id,
                result,
                generation: g,
                ..
            })) => {
                assert_eq!(g, generation);
                *counts.entry(account_id.clone()).or_insert(0) += 1;
                let err_or_ok = match result {
                    Ok(_) => "ok".to_string(),
                    Err(e) => e,
                };
                assert_eq!(counts[&account_id], 1, "duplicate for {account_id}");
                seen.insert(account_id, err_or_ok);
            }
            Ok(Some(_)) => panic!("unexpected msg"),
            _ => break,
        }
    }
    assert_eq!(
        seen.len(),
        3,
        "each student at most one result and all unresolved get failure, got {seen:?}"
    );
    let sanitized = seen
        .values()
        .filter(|v| *v == "qr: internal worker failed")
        .count();
    assert!(
        sanitized >= 2,
        "at least the unresolved students must be sanitized, got {seen:?}"
    );
    for (id, err) in &seen {
        if err.contains("injected") {
            panic!("panic payload leaked for {id}: {err}");
        }
    }
    group.cancel();
}

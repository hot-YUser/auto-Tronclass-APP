//! A quiz's life in the actor: detected, a paper prepared for each participant, reviewed during
//! the countdown, then submitted once per participant.

use super::actor::Env;
use super::{
    Account, ActivityKey, MonitorConfig, MonitorMsg, MonitorPlan, QuizDetected, RuntimeEvent,
};
use crate::json;
use crate::llm::LlmConfig;
use crate::store::config::TargetId;
use crate::tronclass::answer::{self, Source};
use crate::tronclass::quiz::Answer;
use crate::tronclass::Endpoints;
use crate::wire::{
    emit, AccountResultPhase, AnswerWire, AttemptState, CountdownScope, Event, EventCb,
    ExpectedAccount, LogLevel, PreparedAccount, PreparedQuestion, Problem, QuestionOption,
    QuizActivityRef, QuizPrepared,
};
use reqwest::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

pub(super) struct QuizActivity {
    pub(super) activity_token: String,
    pub(super) source: Source,
    pub(super) course: String,
    pub(super) course_id: String, // course the tool executor searches for materials
    pub(super) activity_id: String,
    pub(super) stem: String, // homework question stem, from the detection payload
    pub(super) attempts: HashMap<String, PerAccountAttempt>,
    pub(super) countdown_deadline: Option<Instant>,
    pub(super) held: bool,
    pub(super) discarded: bool,
    pub(super) sources: HashSet<TargetId>,
    pub(super) plan_generation: u64,
    pub(super) mutation_blocked: bool,
}

pub(super) struct PerAccountAttempt {
    pub(super) state: AttemptState,
    pub(super) prepare_generation: u64,
    pub(super) prepare_at: Instant,
    pub(super) prepare_deadline: Option<Instant>,
    pub(super) answer_contract: Option<Vec<Value>>,
    pub(super) instance_id: String,
    pub(super) subjects: Vec<Value>,
    pub(super) generated_answers: HashMap<String, Answer>,
    pub(super) existing_answers: HashMap<String, Answer>,
    pub(super) overrides: HashMap<String, Answer>,
    pub(super) conflicts: HashSet<String>,
    /// Classroom exams submit one subject per request. Preserve confirmed successes so a retry
    /// never resends subjects that the server already accepted.
    pub(super) submitted_subjects: HashSet<String>,
}

impl QuizActivity {
    /// A submission may already be on the wire or accepted. Nothing can take it back.
    pub(super) fn submission_begun(&self) -> bool {
        self.attempts.values().any(|attempt| {
            matches!(
                attempt.state,
                AttemptState::Submitting | AttemptState::Submitted
            )
        })
    }
}

impl PerAccountAttempt {
    fn waiting(now: Instant) -> Self {
        Self {
            state: AttemptState::Waiting,
            prepare_generation: 0,
            prepare_at: now + Duration::from_millis(1200),
            prepare_deadline: None,
            answer_contract: None,
            instance_id: String::new(),
            subjects: Vec::new(),
            generated_answers: HashMap::new(),
            existing_answers: HashMap::new(),
            overrides: HashMap::new(),
            conflicts: HashSet::new(),
            submitted_subjects: HashSet::new(),
        }
    }
}

pub(crate) struct QuizSubmitReport {
    pub(super) detail: String,
    pub(super) completed_subjects: Vec<String>,
    pub(super) warning: Option<String>,
}

pub(crate) struct QuizSubmitFailure {
    pub(super) error: String,
    pub(super) completed_subjects: Vec<String>,
}

pub(crate) struct PreparedAttempt {
    pub(super) account_id: String,
    pub(super) generation: u64,
    pub(super) instance_id: String,
    pub(super) subjects: Vec<Value>,
    pub(super) generated_answers: HashMap<String, Answer>,
    pub(super) existing_answers: HashMap<String, Answer>,
}

/// A paper that could not be fully answered yet, with what was.
pub(crate) struct PrepareRetry {
    pub(super) key: ActivityKey,
    pub(super) account_id: String,
    pub(super) generation: u64,
    pub(super) contract: Vec<Value>,
    pub(super) partial: HashMap<String, Answer>,
    pub(super) missing: Vec<String>,
}

#[derive(Clone)]
pub(super) struct ReusableAnswers {
    pub(super) contract: Vec<Value>,
    pub(super) answers: HashMap<String, Answer>,
}

#[derive(Clone)]
pub(super) struct PriorAnswers {
    pub(super) contract: Vec<Value>,
    pub(super) answers: HashMap<String, Answer>,
}

/// Merge a detection into its quiz. Each participant new to the quiz gets an attempt of its
/// own, and a countdown already running waits for it.
pub(super) fn on_detected(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    plan: &MonitorPlan,
    detected: QuizDetected,
) {
    let routes: Vec<_> = plan
        .routes_for(&detected.account_id, Some(&detected.course_id))
        .collect();
    if routes.is_empty() {
        return;
    }
    let key = ActivityKey::quiz(&detected.base_url, detected.source, &detected.activity_id);
    let generation = detected.generation;
    let quiz = quizzes.entry(key).or_insert_with(|| QuizActivity {
        activity_token: crate::store::config::new_id(),
        source: detected.source,
        course: detected.course,
        course_id: detected.course_id,
        activity_id: detected.activity_id,
        stem: detected.stem,
        attempts: HashMap::new(),
        countdown_deadline: None,
        held: false,
        discarded: false,
        sources: HashSet::new(),
        plan_generation: generation,
        mutation_blocked: false,
    });
    quiz.sources.extend(
        routes
            .iter()
            .flat_map(|route| &route.source_targets)
            .cloned(),
    );
    if !quiz.submission_begun() {
        quiz.plan_generation = generation;
    }
    if quiz.discarded {
        return;
    }
    for participant in routes
        .iter()
        .flat_map(|route| &route.participant_account_ids)
    {
        if !quiz.attempts.contains_key(participant) {
            quiz.attempts.insert(
                participant.clone(),
                PerAccountAttempt::waiting(Instant::now()),
            );
            quiz.countdown_deadline = None;
        }
    }
}

pub(super) fn on_prepared(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    env: &Env,
    key: &ActivityKey,
    prepared: Vec<PreparedAttempt>,
) {
    let Some(q) = quizzes.get_mut(key) else {
        return;
    };
    if q.discarded {
        return;
    }
    for data in prepared {
        let Some(attempt) = q.attempts.get_mut(&data.account_id) else {
            continue;
        };
        if attempt.state != AttemptState::Preparing || attempt.prepare_generation != data.generation
        {
            continue; // stale async completion after a terminal transition
        }
        attempt.answer_contract = Some(paper_contract(&data.subjects));
        attempt.instance_id = data.instance_id;
        attempt.subjects = data.subjects;
        attempt.generated_answers = data.generated_answers;
        attempt.existing_answers = data.existing_answers;
        attempt.overrides.clear();
        attempt.conflicts = attempt
            .existing_answers
            .iter()
            .filter(|(subject_id, existing)| {
                attempt
                    .generated_answers
                    .get(*subject_id)
                    .is_some_and(|generated| generated != *existing)
            })
            .map(|(subject_id, _)| subject_id.clone())
            .collect();
        attempt.state = AttemptState::Ready;
    }
    emit_prepared(env.cb, q);
    rearm_countdown(q, &env.cfg);
}

/// The paper could not be fully answered yet. Keep what was answered and try again after a
/// pause, until the retry budget runs out; then the account's attempt has failed.
pub(super) fn on_prepare_retry(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    env: &Env,
    retry: PrepareRetry,
) {
    let PrepareRetry {
        key,
        account_id,
        generation,
        contract,
        partial,
        missing,
    } = retry;
    let Some(q) = quizzes.get_mut(&key) else {
        return;
    };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(&account_id) else {
        return;
    };
    if attempt.state != AttemptState::Preparing || attempt.prepare_generation != generation {
        return;
    }
    attempt.answer_contract = Some(contract);
    attempt.generated_answers = partial;
    let now = Instant::now();
    let deadline = *attempt
        .prepare_deadline
        .get_or_insert_with(|| now + Duration::from_secs(env.cfg.prepare_retry_budget_secs));
    if now >= deadline {
        attempt.state = AttemptState::Failed;
        let detail = if missing.is_empty() {
            "could not fetch the paper".to_string()
        } else {
            format!("unanswerable subjects: {}", missing.join(", "))
        };
        emit(
            env.cb,
            Problem::error("quiz_unanswerable", format!("{}: {detail}", q.activity_id))
                .activity(&q.activity_token)
                .account(&account_id),
        );
        rearm_countdown(q, &env.cfg);
        emit_prepared(env.cb, q); // publish the terminal account state to the UI completion gate
        return;
    }
    attempt.state = AttemptState::Waiting;
    attempt.prepare_at = now + Duration::from_secs(env.cfg.poll_idle_secs.max(1));
    q.countdown_deadline = None;
}

/// The activity closed before the account's paper could be fetched.
pub(super) fn on_prepare_gone(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    env: &Env,
    key: &ActivityKey,
    account_id: &str,
    generation: u64,
) {
    let Some(q) = quizzes.get_mut(key) else {
        return;
    };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(account_id) else {
        return;
    };
    if attempt.state == AttemptState::Preparing && attempt.prepare_generation == generation {
        attempt.state = AttemptState::Gone;
        rearm_countdown(q, &env.cfg);
        emit_prepared(env.cb, q); // no later Ready event may exist
    }
}

pub(super) fn on_prepare_failed(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    env: &Env,
    key: &ActivityKey,
    account_id: &str,
    generation: u64,
    code: &str,
    message: String,
) {
    let Some(q) = quizzes.get_mut(key) else {
        return;
    };
    if q.discarded {
        return;
    }
    let Some(attempt) = q.attempts.get_mut(account_id) else {
        return;
    };
    if attempt.state != AttemptState::Preparing || attempt.prepare_generation != generation {
        return;
    }
    attempt.state = AttemptState::Failed;
    emit(
        env.cb,
        Problem::error(code, message)
            .activity(&q.activity_token)
            .account(account_id),
    );
    rearm_countdown(q, &env.cfg);
    emit_prepared(env.cb, q); // no later Ready event may exist
}

/// Stop the countdown and keep the quiz for the user to submit. Refused once a submission has
/// begun: it cannot be taken back, and holding must not pretend otherwise.
pub(super) fn hold(q: &mut QuizActivity) -> Result<(), String> {
    if q.submission_begun() {
        return Err("submission has begun".to_string());
    }
    q.countdown_deadline = None;
    q.held = true;
    Ok(())
}

/// Give the quiz up. Refused once a submission has begun, for the same reason as `hold`.
pub(super) fn discard(q: &mut QuizActivity, cb: EventCb) -> Result<(), String> {
    if q.submission_begun() {
        return Err("submission has begun".to_string());
    }
    q.countdown_deadline = None;
    q.discarded = true;
    emit(
        cb,
        Event::LogLine {
            level: LogLevel::Info,
            text: format!("quiz {} discarded", q.activity_id),
            activity_token: Some(&q.activity_token),
        },
    );
    Ok(())
}

pub(super) fn rearm_countdown(q: &mut QuizActivity, cfg: &MonitorConfig) {
    let preparation_pending = q.attempts.values().any(|attempt| {
        matches!(
            attempt.state,
            AttemptState::Waiting | AttemptState::Preparing
        )
    });
    let unresolved_conflict = q
        .attempts
        .values()
        .any(|attempt| !attempt.conflicts.is_empty());
    let has_ready = q
        .attempts
        .values()
        .any(|attempt| attempt.state == AttemptState::Ready);
    if q.held || q.discarded || preparation_pending || unresolved_conflict || !has_ready {
        q.countdown_deadline = None;
    } else if q.countdown_deadline.is_none() {
        q.countdown_deadline = Some(Instant::now() + Duration::from_secs(cfg.countdown_secs));
    }
}

/// The user's own answer for one subject. It overrides the generated one and settles a conflict.
pub(super) fn set_answer(
    q: &mut QuizActivity,
    env: &Env,
    account_id: &str,
    subject_id: &str,
    answer: AnswerWire,
) -> Result<(), String> {
    let attempt = q
        .attempts
        .get_mut(account_id)
        .ok_or_else(|| "account is not a participant in this quiz".to_string())?;
    if attempt.state != AttemptState::Ready {
        return Err("account paper is not ready for answer changes".to_string());
    }
    let subject = attempt
        .subjects
        .iter()
        .find(|subject| crate::tronclass::quiz::subject_id(subject) == subject_id)
        .ok_or_else(|| "unknown subject for this quiz".to_string())?;
    let answer = answer.into_answer()?;
    crate::tronclass::quiz::validate_answer(subject, &answer, q.source == Source::Vote)?;
    attempt
        .overrides
        .insert(subject_id.to_string(), answer.clone());
    attempt.conflicts.remove(subject_id);
    let answer = AnswerWire::from_answer(&answer);
    emit(
        env.cb,
        Event::AnswerUpdated {
            quiz_id: &q.activity_id,
            activity_token: &q.activity_token,
            account_id,
            subject_id,
            display_answer: answer.display(),
            answer,
            source: "user",
            conflict: false,
        },
    );
    rearm_countdown(q, &env.cfg);
    Ok(())
}

/// Once a second: start preparing the attempts that are due, and run each quiz's countdown.
pub(super) fn on_tick(quizzes: &mut HashMap<ActivityKey, QuizActivity>, env: &Env, now: Instant) {
    let keys: Vec<ActivityKey> = quizzes.keys().cloned().collect();
    for key in keys {
        let Some(q) = quizzes.get_mut(&key) else {
            continue;
        };
        if q.discarded {
            continue; // discard must stop queued/retry preparation before any LLM/network work starts
        }
        let mut due_ids: Vec<String> = q
            .attempts
            .iter()
            .filter(|(_, attempt)| {
                attempt.state == AttemptState::Waiting && now >= attempt.prepare_at
            })
            .map(|(account_id, _)| account_id.clone())
            .collect();
        due_ids.sort();
        if !due_ids.is_empty() {
            for account_id in due_ids
                .iter()
                .filter(|account_id| !env.accounts.contains_key(*account_id))
            {
                if let Some(attempt) = q.attempts.get_mut(account_id) {
                    attempt.state = AttemptState::Failed;
                }
                emit(
                    env.cb,
                    Problem::error("quiz_account_unavailable", "測驗帳號已不在監控工作階段中")
                        .activity(&q.activity_token)
                        .account(account_id),
                );
            }
            due_ids.retain(|account_id| env.accounts.contains_key(account_id));
            if due_ids.is_empty() {
                rearm_countdown(q, &env.cfg);
                continue;
            }
            let mut participants: Vec<Arc<Account>> = Vec::new();
            for account_id in &due_ids {
                let Some(account) = env.accounts.get(account_id) else {
                    // Fail closed (defensive — the retain above already filtered): a participant
                    // missing from the session must never panic the actor. Report the same terminal
                    // state as the pre-filter and skip the account.
                    if let Some(attempt) = q.attempts.get_mut(account_id) {
                        attempt.state = AttemptState::Failed;
                    }
                    emit(
                        env.cb,
                        Problem::error("quiz_account_unavailable", "測驗帳號已不在監控工作階段中")
                            .activity(&q.activity_token)
                            .account(account_id),
                    );
                    continue;
                };
                participants.push(account.clone());
            }
            let priors: HashMap<String, PriorAnswers> = due_ids
                .iter()
                .filter_map(|account_id| {
                    let attempt = q.attempts.get(account_id)?;
                    Some((
                        account_id.clone(),
                        PriorAnswers {
                            contract: attempt.answer_contract.clone()?,
                            answers: attempt.generated_answers.clone(),
                        },
                    ))
                })
                .collect();
            let reusable: Vec<ReusableAnswers> = if env.cfg.enable_llm_tools {
                Vec::new()
            } else {
                q.attempts
                    .values()
                    .filter(|attempt| {
                        matches!(
                            attempt.state,
                            AttemptState::Ready
                                | AttemptState::Submitting
                                | AttemptState::Submitted
                        )
                    })
                    .map(|attempt| ReusableAnswers {
                        contract: paper_contract(&attempt.subjects),
                        answers: attempt.generated_answers.clone(),
                    })
                    .collect()
            };
            let mut generations = HashMap::new();
            for account_id in &due_ids {
                if let Some(attempt) = q.attempts.get_mut(account_id) {
                    attempt.prepare_generation = attempt.prepare_generation.wrapping_add(1).max(1);
                    attempt.state = AttemptState::Preparing;
                    generations.insert(account_id.clone(), attempt.prepare_generation);
                }
            }
            q.countdown_deadline = None;
            spawn_prepare(
                env,
                PrepareJob {
                    participants,
                    source: q.source,
                    activity_id: q.activity_id.clone(),
                    activity_token: q.activity_token.clone(),
                    course_id: q.course_id.clone(),
                    stem: q.stem.clone(),
                    llm: env.cfg.llm(),
                    max_reask: env.cfg.max_answer_reask,
                    prepare_budget_secs: env.cfg.prepare_retry_budget_secs,
                    priors,
                    generations,
                    reusable,
                    key: key.clone(),
                },
            );
            continue;
        }

        let Some(deadline) = q.countdown_deadline else {
            continue;
        };
        let remaining = deadline.saturating_duration_since(now).as_secs();
        emit(
            env.cb,
            Event::Countdown {
                scope: CountdownScope::Quiz,
                activity_token: &q.activity_token,
                external_id: &q.activity_id,
                remaining_secs: remaining,
            },
        );
        if now >= deadline {
            let _ = dispatch_submits(quizzes, env, &key);
        }
    }
}

/// Submit every ready attempt. Refused while a paper is still being prepared or an answer
/// conflict is unresolved.
pub(super) fn dispatch_submits(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    env: &Env,
    key: &ActivityKey,
) -> Result<(), String> {
    let Some(q) = quizzes.get_mut(key) else {
        return Err("unknown quiz activity".to_string());
    };
    if q.mutation_blocked {
        return Err("definition_change_in_progress".to_string());
    }
    if q.discarded {
        return Err("quiz was discarded".to_string());
    }
    if q.attempts.values().any(|attempt| {
        matches!(
            attempt.state,
            AttemptState::Waiting | AttemptState::Preparing
        )
    }) {
        return Err("quiz attempts are still preparing".to_string());
    }
    if q.attempts
        .values()
        .any(|attempt| !attempt.conflicts.is_empty())
    {
        return Err("quiz has unresolved answer conflicts".to_string());
    }
    q.countdown_deadline = None;
    let source = q.source;
    let resubmit = env.cfg.resubmit_for_correct;
    let activity_id = q.activity_id.clone();
    let ready_ids: Vec<String> = q
        .attempts
        .iter()
        .filter(|(_, attempt)| attempt.state == AttemptState::Ready)
        .map(|(account_id, _)| account_id.clone())
        .collect();
    if ready_ids.is_empty() {
        return Err("quiz has no ready attempts to submit".to_string());
    }
    let mut jobs = Vec::with_capacity(ready_ids.len());
    for account_id in &ready_ids {
        let account = env
            .accounts
            .get(account_id)
            .cloned()
            .ok_or_else(|| format!("quiz account {account_id} is unavailable"))?;
        let Some(attempt) = q.attempts.get(account_id) else {
            // Fail closed (defensive — ready_ids came from the same map): a vanished attempt must
            // not be submitted from stale state, nor wedged into Submitting below.
            continue;
        };
        let mut answers = attempt.generated_answers.clone();
        for (subject_id, answer) in &attempt.overrides {
            answers.insert(subject_id.clone(), answer.clone());
        }
        jobs.push(SubmitJob {
            account,
            source,
            activity_id: activity_id.clone(),
            instance_id: attempt.instance_id.clone(),
            subjects: attempt.subjects.clone(),
            answers,
            submitted_subjects: attempt.submitted_subjects.clone(),
            resubmit,
        });
    }
    for account_id in ready_ids {
        if let Some(attempt) = q.attempts.get_mut(&account_id) {
            attempt.state = AttemptState::Submitting;
        }
    }
    for job in jobs {
        spawn_submit(env, key, job);
    }
    Ok(())
}

pub(super) fn on_submit_result(
    quizzes: &mut HashMap<ActivityKey, QuizActivity>,
    env: &Env,
    key: &ActivityKey,
    account_id: &str,
    result: Result<QuizSubmitReport, QuizSubmitFailure>,
) {
    let Some(q) = quizzes.get_mut(key) else {
        return;
    };
    publish(env, q, account_id, &result);
    let Some(attempt) = q.attempts.get_mut(account_id) else {
        return;
    };
    match result {
        Ok(report) => {
            attempt.submitted_subjects.extend(report.completed_subjects);
            attempt.state = AttemptState::Submitted;
            emit(
                env.cb,
                Event::QuizSubmitted {
                    quiz_id: &q.activity_id,
                    activity_token: &q.activity_token,
                    account_id,
                    result: &report.detail,
                },
            );
            if let Some(warning) = report.warning {
                emit(
                    env.cb,
                    Problem::warning("quiz_correction_failed", warning)
                        .activity(&q.activity_token)
                        .account(account_id),
                );
            }
        }
        Err(failure) => {
            attempt
                .submitted_subjects
                .extend(failure.completed_subjects);
            // No unconditional auto-retry of an ambiguous mutation (it could duplicate the submit);
            // the attempt stays Ready so the user can explicitly SubmitNow, and the message says so.
            attempt.state = AttemptState::Ready;
            emit(
                env.cb,
                Problem::error(
                    "quiz_submit_failed",
                    format!(
                        "{}: {}（可手動重試：於測驗頁按「立即送出」再次提交）",
                        account_id, failure.error
                    ),
                )
                .activity(&q.activity_token)
                .account(account_id),
            );
        }
    }
}

/// Tell the engine how one account fared, for the monitoring snapshot.
fn publish(
    env: &Env,
    quiz: &QuizActivity,
    account_id: &str,
    result: &Result<QuizSubmitReport, QuizSubmitFailure>,
) {
    let (phase, error) = match result {
        Ok(_) => (AccountResultPhase::Succeeded, None),
        Err(failure) => (AccountResultPhase::Failed, Some(failure.error.clone())),
    };
    let _ = env.runtime_tx.send(RuntimeEvent::AccountResult {
        sources: quiz.sources.iter().cloned().collect(),
        account_id: account_id.to_string(),
        phase,
        activity_kind: quiz.source.as_str().to_string(),
        course_name: quiz.course.clone(),
        error,
    });
}

/// One round of paper preparation, for the accounts whose attempt is due.
pub(super) struct PrepareJob {
    pub(super) participants: Vec<Arc<Account>>,
    pub(super) source: Source,
    pub(super) activity_id: String,
    pub(super) activity_token: String,
    pub(super) course_id: String,
    pub(super) stem: String,
    pub(super) llm: LlmConfig,
    pub(super) max_reask: u32,
    /// Seconds one account's answering may take in this round; zero means no cap.
    pub(super) prepare_budget_secs: u64,
    /// What each account had answered before, so a retry resumes rather than restarts.
    pub(super) priors: HashMap<String, PriorAnswers>,
    pub(super) generations: HashMap<String, u64>,
    /// Answers already generated for an identical paper, shared between accounts.
    pub(super) reusable: Vec<ReusableAnswers>,
    pub(super) key: ActivityKey,
}

pub(super) fn spawn_prepare(env: &Env, job: PrepareJob) {
    let (tx, cb) = (env.tx.clone(), env.cb);
    env.spawn(async move {
        let PrepareJob {
            participants,
            source,
            activity_id,
            activity_token,
            course_id,
            stem,
            llm,
            max_reask,
            prepare_budget_secs,
            priors,
            generations,
            mut reusable,
            key,
        } = job;
        // The LLM's own cookie-less client — school cookies must never reach the model endpoint.
        let llm_client = match crate::llm::build_client() {
            Ok(client) => client,
            Err(error) => {
                for account in &participants {
                    tx.send(MonitorMsg::QuizPrepareFailed {
                        key: key.clone(),
                        account_id: account.id.clone(),
                        generation: generations.get(&account.id).copied().unwrap_or_default(),
                        code: "llm_client_unavailable".to_string(),
                        message: format!("LLM 客戶端初始化失敗：{error}"),
                    })
                    .ok();
                }
                return;
            }
        };
        let mut prepared = Vec::new();
        for account in participants {
            let account_id = account.id.clone();
            let generation = generations.get(&account_id).copied().unwrap_or_default();
            let prior_snapshot = priors.get(&account_id).cloned();
            let endpoints = Endpoints::derive(&account.base_url);
            let paper =
                match answer::fetch_paper(&account.client, &endpoints, source, &activity_id, &stem)
                    .await
                {
                    Ok(paper) => paper,
                    Err(_) => {
                        tx.send(MonitorMsg::QuizPrepareRetry(PrepareRetry {
                            key: key.clone(),
                            account_id,
                            generation,
                            contract: prior_snapshot
                                .as_ref()
                                .map(|prior| prior.contract.clone())
                                .unwrap_or_default(),
                            partial: prior_snapshot
                                .as_ref()
                                .map(|prior| prior.answers.clone())
                                .unwrap_or_default(),
                            missing: Vec::new(),
                        }))
                        .ok();
                        continue;
                    }
                };
            if paper.subjects.is_empty() {
                tx.send(MonitorMsg::QuizPrepareGone {
                    key: key.clone(),
                    account_id,
                    generation,
                })
                .ok();
                continue;
            }

            let contract = paper_contract(&paper.subjects);
            let prior = compatible_prior(prior_snapshot.as_ref(), &contract);
            let generated_answers = if prior.is_empty() && !llm.enable_tools {
                reusable
                    .iter()
                    .find(|entry| entry.contract == contract)
                    .map(|entry| entry.answers.clone())
            } else {
                None
            };
            let generated_answers = match generated_answers {
                Some(answers) => answers,
                None => {
                    let answering = answer::shared_answers(
                        &account.client,
                        &llm_client,
                        &llm,
                        cb,
                        &activity_token,
                        &account_id,
                        &course_id,
                        &account.base_url,
                        &paper.subjects,
                        max_reask,
                        &prior,
                    );
                    let result = if prepare_budget_secs > 0 {
                        tokio::time::timeout(Duration::from_secs(prepare_budget_secs), answering)
                            .await
                    } else {
                        Ok(answering.await)
                    };
                    match result {
                        Ok(answers) => answers,
                        Err(_) => {
                            // The overall budget ran out mid-answer: hand the actor the existing
                            // partial state so its retry-deadline path turns this into
                            // QuizPrepareFailed — the attempt must never sit in Preparing forever.
                            tx.send(MonitorMsg::QuizPrepareRetry(PrepareRetry {
                                key: key.clone(),
                                account_id,
                                generation,
                                contract,
                                partial: prior.clone(),
                                missing: answer::missing_subjects(&paper.subjects, &prior),
                            }))
                            .ok();
                            continue;
                        }
                    }
                }
            };
            let missing = answer::missing_subjects(&paper.subjects, &generated_answers);
            if !missing.is_empty() {
                if llm.api_key.trim().is_empty() {
                    tx.send(MonitorMsg::QuizPrepareFailed {
                        key: key.clone(),
                        account_id,
                        generation,
                        code: "llm_key_missing".to_string(),
                        message: format!(
                            "{activity_id}：尚未設定 LLM 金鑰，無法自動作答（請至 設定 → 儲存金鑰）"
                        ),
                    })
                    .ok();
                } else {
                    tx.send(MonitorMsg::QuizPrepareRetry(PrepareRetry {
                        key: key.clone(),
                        account_id,
                        generation,
                        contract,
                        partial: generated_answers,
                        missing,
                    }))
                    .ok();
                }
                continue;
            }

            if !llm.enable_tools && !reusable.iter().any(|entry| entry.contract == contract) {
                reusable.push(ReusableAnswers {
                    contract,
                    answers: generated_answers.clone(),
                });
            }
            let existing_answers = paper
                .subjects
                .iter()
                .filter_map(|subject| {
                    answer::existing_answer(subject)
                        .map(|value| (crate::tronclass::quiz::subject_id(subject), value))
                })
                .collect();
            prepared.push(PreparedAttempt {
                account_id,
                generation,
                instance_id: paper.instance_id,
                subjects: paper.subjects,
                generated_answers,
                existing_answers,
            });
        }
        if !prepared.is_empty() {
            tx.send(MonitorMsg::QuizPrepared {
                key,
                attempts: prepared,
            })
            .ok();
        }
    });
}

/// Remove only per-user response fields before comparing paper contracts. Correct-answer leaks remain:
/// they affect the generated answer and therefore must be identical before reuse is safe.
pub(super) fn paper_contract(subjects: &[Value]) -> Vec<Value> {
    subjects
        .iter()
        .cloned()
        .map(|mut subject| {
            if let Some(object) = subject.as_object_mut() {
                for key in [
                    "student_answer_option_ids",
                    "student_answer",
                    "student_answers",
                    "my_answer",
                ] {
                    object.remove(key);
                }
            }
            subject
        })
        .collect()
}

pub(super) fn compatible_prior(
    prior: Option<&PriorAnswers>,
    contract: &[Value],
) -> HashMap<String, Answer> {
    prior
        .filter(|prior| prior.contract == contract)
        .map(|prior| prior.answers.clone())
        .unwrap_or_default()
}

/// One account's submission.
pub(super) struct SubmitJob {
    pub(super) account: Arc<Account>,
    pub(super) source: Source,
    pub(super) activity_id: String,
    pub(super) instance_id: String,
    pub(super) subjects: Vec<Value>,
    pub(super) answers: HashMap<String, Answer>,
    /// Subjects the server has already accepted, never to be sent twice.
    pub(super) submitted_subjects: HashSet<String>,
    pub(super) resubmit: bool,
}

pub(super) fn spawn_submit(env: &Env, key: &ActivityKey, job: SubmitJob) {
    let (tx, key) = (env.tx.clone(), key.clone());
    env.spawn(async move {
        let SubmitJob {
            account: acc,
            source,
            activity_id,
            instance_id,
            subjects,
            answers,
            submitted_subjects,
            resubmit,
        } = job;
        let ep = Endpoints::derive(&acc.base_url);
        let result: Result<QuizSubmitReport, QuizSubmitFailure> = match source {
            Source::Exam => match answer::submit_exam(
                &acc.client,
                &ep,
                &activity_id,
                &instance_id,
                &answers,
                &subjects,
            )
            .await
            {
                Ok((sid, retake)) => {
                    // resubmit gate: EXAM + pref + the SUBMIT RESPONSE's allow_retake_exam + a submission
                    // id — a single-attempt exam must not burn its one graded attempt.
                    if resubmit && retake && !sid.is_empty() {
                        let correction =
                            answer::resubmit_correct(&acc.client, &ep, &activity_id, &sid).await;
                        // The graded initial submit is already committed. A correction failure is
                        // visible but terminal; retrying the initial paper could burn another attempt.
                        Ok(finalize_exam_submit(&sid, correction))
                    } else {
                        Ok(submit_report(format!("submitted {sid}")))
                    }
                }
                Err(error) => Err(submit_failure(error)),
            },
            Source::ClassroomExam => {
                // per-subject POST with the full exam wrapper (flat body → 400). the server also gates
                // on the server's started_subjects_count≥1; here each answered subject is posted.
                submit_classroom(
                    &acc,
                    &ep,
                    &activity_id,
                    &instance_id,
                    &subjects,
                    &answers,
                    &submitted_subjects,
                )
                .await
                .map(|completed_subjects| QuizSubmitReport {
                    detail: "submitted (classroom)".into(),
                    completed_subjects,
                    warning: None,
                })
            }
            Source::Questionnaire => {
                // exam wrapper (NOT courseware), to the questionnaire endpoint.
                let entries: Vec<Value> = subjects
                    .iter()
                    .filter_map(|s| {
                        answers
                            .get(&crate::tronclass::quiz::subject_id(s))
                            .map(|a| answer::exam_subject_entry(s, a))
                    })
                    .collect();
                post_json(
                    &acc.client,
                    &ep.questionnaire_submissions(&activity_id),
                    &answer::questionnaire_body(&instance_id, &entries),
                )
                .await
                .map(|_| submit_report("submitted (questionnaire)"))
                .map_err(submit_failure)
            }
            Source::Vote => {
                let letters: Vec<String> = answers.values().flat_map(vote_letters).collect();
                post_json(
                    &acc.client,
                    &ep.vote_cast(&activity_id),
                    &answer::vote_body(&letters),
                )
                .await
                .map(|_| submit_report("voted"))
                .map_err(submit_failure)
            }
            Source::CoursewareQuiz => {
                let items = source_items(&subjects, &answers);
                post_json(
                    &acc.client,
                    &ep.courseware_submissions(&activity_id),
                    &answer::courseware_body(&items),
                )
                .await
                .map(|_| submit_report("submitted (courseware)"))
                .map_err(submit_failure)
            }
            Source::Homework => {
                let text = answers
                    .values()
                    .filter_map(answer_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                post_json(
                    &acc.client,
                    &ep.homework_submissions(&activity_id),
                    &answer::homework_body(&text),
                )
                .await
                .map(|_| submit_report("submitted (homework)"))
                .map_err(submit_failure)
            }
        };
        tx.send(MonitorMsg::QuizSubmitResult {
            key,
            account_id: acc.id.clone(),
            result,
        })
        .ok();
    });
}

pub(super) fn submit_report(detail: impl Into<String>) -> QuizSubmitReport {
    QuizSubmitReport {
        detail: detail.into(),
        completed_subjects: Vec::new(),
        warning: None,
    }
}

pub(super) fn finalize_exam_submit(sid: &str, correction: Result<(), String>) -> QuizSubmitReport {
    match correction {
        Ok(()) => submit_report(format!("submitted {sid}; corrected resubmit completed")),
        Err(error) => QuizSubmitReport {
            detail: format!("submitted {sid}"),
            completed_subjects: Vec::new(),
            warning: Some(format!(
                "initial submit {sid} succeeded; correction pass failed: {error}"
            )),
        },
    }
}

pub(super) fn submit_failure(error: impl Into<String>) -> QuizSubmitFailure {
    QuizSubmitFailure {
        error: error.into(),
        completed_subjects: Vec::new(),
    }
}

pub(super) fn emit_prepared(cb: EventCb, q: &QuizActivity) {
    emit(cb, quiz_prepared(q));
}

pub(super) fn quiz_prepared(q: &QuizActivity) -> QuizPrepared<'_> {
    let per_account = q
        .attempts
        .iter()
        .filter(|(_, attempt)| {
            matches!(
                attempt.state,
                AttemptState::Ready | AttemptState::Submitting | AttemptState::Submitted
            )
        })
        .map(|(account_id, attempt)| PreparedAccount {
            account_id,
            instance_id: &attempt.instance_id,
            state: attempt.state,
            questions: attempt
                .subjects
                .iter()
                .map(|subject| prepared_question(attempt, subject))
                .collect(),
        })
        .collect();
    QuizPrepared {
        schema_version: 1,
        activity_token: &q.activity_token,
        quiz_id: &q.activity_id,
        activity: QuizActivityRef {
            external_id: &q.activity_id,
            source: q.source.as_str(),
            course_id: &q.course_id,
            course: &q.course,
        },
        course: &q.course,
        per_account,
        expected_accounts: q
            .attempts
            .iter()
            .map(|(account_id, attempt)| ExpectedAccount {
                account_id,
                state: attempt.state,
            })
            .collect(),
        conflict_count: q
            .attempts
            .values()
            .map(|attempt| attempt.conflicts.len())
            .sum(),
    }
}

pub(super) fn prepared_question<'a>(
    attempt: &'a PerAccountAttempt,
    subject: &'a Value,
) -> PreparedQuestion<'a> {
    let subject_id = crate::tronclass::quiz::subject_id(subject);
    let answer = attempt
        .generated_answers
        .get(&subject_id)
        .map(AnswerWire::from_answer);
    PreparedQuestion {
        parent_id: subject.get("parent_id").and_then(json::id),
        kind: json::text_at(subject, &["type"]),
        answer_type: json::text_at(subject, &["answer_type", "type"]),
        stem: crate::tronclass::quiz::stem(subject),
        options: json::array_at(subject, &["options"])
            .iter()
            .map(|option| QuestionOption {
                id: crate::tronclass::quiz::option_id(option),
                text: json::text_at(option, &["content", "description"]),
            })
            .collect(),
        display_answer: answer.as_ref().map(AnswerWire::display).unwrap_or_default(),
        answer,
        existing_answer: attempt
            .existing_answers
            .get(&subject_id)
            .map(AnswerWire::from_answer),
        source: "llm",
        conflict: attempt.conflicts.contains(&subject_id),
        subject_id,
    }
}

pub(super) fn find_key(
    quizzes: &HashMap<ActivityKey, QuizActivity>,
    activity_token: &str,
) -> Result<ActivityKey, String> {
    quizzes
        .iter()
        .find(|(_, quiz)| quiz.activity_token == activity_token)
        .map(|(key, _)| key.clone())
        .ok_or_else(|| "unknown quiz activity_token".to_string())
}

pub(super) fn find_mut<'a>(
    quizzes: &'a mut HashMap<ActivityKey, QuizActivity>,
    activity_token: &str,
) -> Result<&'a mut QuizActivity, String> {
    quizzes
        .values_mut()
        .find(|quiz| quiz.activity_token == activity_token)
        .ok_or_else(|| "unknown quiz activity_token".to_string())
}

pub(super) async fn post_json(client: &Client, url: &str, body: &Value) -> Result<(), String> {
    // mutation_checked is the ONE 2xx+business-error gate (the shared envelope parser owns success
    // semantics). A 2xx JSON body without an explicit business error is a success — no extra local
    // boolean gate: responses like `{"id":…}` would otherwise be rejected despite a committed submit.
    crate::http::mutation_checked(client.post(url).json(body), "submit activity")
        .await
        .map(|_| ())
}

pub(super) async fn submit_classroom(
    account: &Account,
    endpoints: &Endpoints,
    activity_id: &str,
    instance_id: &str,
    subjects: &[Value],
    answers: &HashMap<String, Answer>,
    already_submitted: &HashSet<String>,
) -> Result<Vec<String>, QuizSubmitFailure> {
    let mut completed = Vec::new();
    let mut answered = 0_usize;
    for subject in subjects {
        let subject_id = crate::tronclass::quiz::subject_id(subject);
        let Some(answer) = answers.get(&subject_id) else {
            continue;
        };
        answered += 1;
        if already_submitted.contains(&subject_id) {
            continue;
        }
        let body = answer::classroom_body(instance_id, subject, answer);
        let operation = format!("submit classroom subject {subject_id}");
        // mutation_checked is the single 2xx+business-error gate (shared envelope parser) — the same
        // rule as post_json. Confirmed subjects are retained so a retry never resends them.
        if let Err(error) = crate::http::mutation_checked(
            account
                .client
                .post(endpoints.classroom_submit(activity_id, &subject_id))
                .json(&body),
            &operation,
        )
        .await
        {
            return Err(QuizSubmitFailure {
                error,
                completed_subjects: completed,
            });
        }
        completed.push(subject_id);
    }
    if answered == 0 {
        return Err(submit_failure("submit classroom: no answered subjects"));
    }
    Ok(completed)
}

pub(super) fn vote_letters(a: &Answer) -> Vec<String> {
    match a {
        Answer::Vote(l) => l.clone(),
        Answer::Options(o) => o.clone(),
        _ => vec![],
    }
}

pub(super) fn answer_text(a: &Answer) -> Option<String> {
    match a {
        Answer::Text(t) => Some(t.clone()),
        Answer::Blanks(b) => Some(b.join(" ")),
        _ => None,
    }
}

/// (subject_id, answer_type, answer) for each answered subject — courseware's `subjects_answers` needs
/// all three (the answer_type falls back to the subject `type`).
pub(super) fn source_items(
    subjects: &[Value],
    answers: &HashMap<String, Answer>,
) -> Vec<(String, String, Answer)> {
    subjects
        .iter()
        .filter_map(|s| {
            let sid = crate::tronclass::quiz::subject_id(s);
            let atype = s
                .get("answer_type")
                .and_then(Value::as_str)
                .or_else(|| s.get("type").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            answers.get(&sid).map(|a| (sid, atype, a.clone()))
        })
        .collect()
}

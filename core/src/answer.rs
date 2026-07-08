//! Auto-answer flow helpers (docs 31): fetch a paper, build the LLM prompt, parse the answer, detect
//! an account's existing answer, and build the **exact per-source submission body**. The orchestration
//! (prepare → conflict → countdown → submit) lives in `monitor.rs`; the network + pure builders live here.
//!
//! Submit values are VERBATIM (gotcha 1): blank/short answers keep whatever text/HTML they carry.

use crate::llm::{self, LlmConfig};
use crate::providers::Endpoints;
use crate::quiz::{self, Answer, Decision};
use reqwest::Client;
use serde_json::{json, Value};

/// Which activity family; picks the fetch + submit contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Exam,
    Vote,
    CoursewareQuiz,
    ClassroomExam,
    Homework,
    Questionnaire,
}

impl Source {
    pub fn parse(s: &str) -> Source {
        match s {
            "vote" | "interaction" => Source::Vote,
            "courseware-quiz" | "courseware_quiz" => Source::CoursewareQuiz,
            "classroom-exam" | "classroom_exam" => Source::ClassroomExam,
            "homework" => Source::Homework,
            "questionnaire" => Source::Questionnaire,
            _ => Source::Exam,
        }
    }
}

pub struct Paper {
    pub instance_id: String,
    pub subjects: Vec<Value>,
    pub allow_retake: bool,
    pub reveal: bool,
}

/// Fetch the paper for an activity. Exam does qualification → distribute; other sources read their
/// own subjects. (`/distribute` is fetch-paper only — detection is per-course, see `monitor.rs`.)
pub async fn fetch_paper(client: &Client, ep: &Endpoints, source: Source, activity_id: &str) -> Result<Paper, String> {
    let url = match source {
        Source::Exam | Source::ClassroomExam => {
            let _ = client.get(ep.exam_qualification(activity_id)).send().await; // best-effort gate
            ep.exam_distribute(activity_id)
        }
        Source::CoursewareQuiz => ep.courseware_subjects(activity_id),
        Source::Vote | Source::Homework | Source::Questionnaire => ep.exam_distribute(activity_id),
    };
    let v: Value = client.get(url).send().await.map_err(|e| format!("distribute: {e}"))?.json().await.map_err(|e| e.to_string())?;
    Ok(Paper {
        instance_id: v.get("exam_paper_instance_id").and_then(Value::as_str).unwrap_or("").to_string(),
        subjects: v.get("subjects").and_then(Value::as_array).cloned().unwrap_or_default(),
        allow_retake: v.get("allow_retake_exam").and_then(Value::as_bool).unwrap_or(false),
        reveal: v.get("announce_answer").and_then(Value::as_str) == Some("immediate"),
    })
}

/// Decide every subject; for pending ones ask the LLM (streaming reasoning). Shared answers, run once
/// per activity. Blank/pending subjects re-asked up to `max_reask`; a persistent empty is dropped
/// (never submit blank).
pub async fn shared_answers(
    client: &Client,
    cfg: &LlmConfig,
    cb: llm::EventCb,
    quiz_id: &str,
    subjects: &[Value],
    max_reask: u32,
) -> std::collections::HashMap<String, Answer> {
    let mut answers = std::collections::HashMap::new();
    for plan in quiz::decide_paper(subjects) {
        match plan.decision {
            Decision::Skip => {}
            Decision::Replay(a) => {
                answers.insert(plan.subject_id, a);
            }
            Decision::Pending => {
                if let Some(subject) = subjects.iter().find(|s| quiz::subject_id(s) == plan.subject_id) {
                    let prompt = build_prompt(subject);
                    for _ in 0..max_reask.max(1) {
                        if let Some(text) = llm::answer_question(client, cfg, &prompt, cb, quiz_id, &plan.subject_id).await {
                            if let Some(a) = parse_answer(subject, &plan.qtype, &text) {
                                answers.insert(plan.subject_id.clone(), a);
                                break;
                            }
                        }
                    }
                    // Still nothing → leave it out; the submit skips it (never blank).
                }
            }
        }
    }
    answers
}

fn build_prompt(subject: &Value) -> String {
    let stem = quiz::clean_html(subject.get("content").and_then(Value::as_str).unwrap_or(""));
    let mut p = format!("Answer this question. Question: {stem}\n");
    if let Some(opts) = subject.get("options").and_then(Value::as_array) {
        p.push_str("Options:\n");
        for o in opts {
            let id = o.get("id").and_then(Value::as_str).unwrap_or("");
            let c = quiz::clean_html(o.get("content").and_then(Value::as_str).unwrap_or(""));
            p.push_str(&format!("- [{id}] {c}\n"));
        }
        p.push_str("Reply with the option id(s) only.");
    } else {
        p.push_str("Reply with the answer text only.");
    }
    p
}

/// Map the LLM's text back to a concrete answer for the subject's type.
fn parse_answer(subject: &Value, qtype: &str, text: &str) -> Option<Answer> {
    match qtype {
        "single_selection" | "multiple_selection" | "true_or_false" => {
            let ids: Vec<String> = subject
                .get("options")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|o| o.get("id").and_then(Value::as_str))
                .filter(|id| text.contains(*id))
                .map(str::to_string)
                .collect();
            (!ids.is_empty()).then_some(Answer::Options(ids))
        }
        t if quiz::BLANK_TYPES.contains(&t) => {
            Some(Answer::Blanks(text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()))
        }
        _ => Some(Answer::Text(text.trim().to_string())),
    }
}

/// An account's existing answer for a subject (parsed from its distribute), for conflict detection.
pub fn existing_answer(subject: &Value) -> Option<Answer> {
    if let Some(ids) = subject.get("student_answer_option_ids").and_then(Value::as_array) {
        let ids: Vec<String> = ids.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
        if !ids.is_empty() {
            return Some(Answer::Options(ids));
        }
    }
    subject
        .get("student_answer")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| Answer::Text(s.to_string()))
}

// --- pure per-source submission bodies (unit-tested; shapes are docs 31 + corrections) ---

/// One subject's answer as an exam-wrapper subject entry.
pub fn exam_subject_entry(subject_id: &str, answer: &Answer) -> Value {
    let mut e = json!({ "subject_id": subject_id, "answer_option_ids": [], "answer": "" });
    match answer {
        Answer::Options(ids) => e["answer_option_ids"] = json!(ids),
        Answer::Text(t) => e["answer"] = json!(t),                 // verbatim
        Answer::Blanks(b) => e["answers"] = json!(b),              // per-blank, verbatim
        Answer::Matching(pairs) => e["answer_option_ids"] = json!(pairs.iter().map(|(_, o)| o).collect::<Vec<_>>()),
        Answer::Vote(_) => {}
    }
    e
}

pub fn exam_body(instance_id: &str, entries: &[Value]) -> Value {
    json!({ "exam_paper_instance_id": instance_id, "examFinished": true, "subjects": entries })
}

pub fn vote_body(letters: &[String]) -> Value {
    json!({ "votes": letters })
}

pub fn courseware_body(items: &[(String, Answer)]) -> Value {
    let arr: Vec<Value> = items
        .iter()
        .map(|(qtype, a)| json!({ "type": qtype, "answers": answer_texts(a) }))
        .collect();
    json!({ "subjects_answers": arr })
}

/// classroom-exam: full exam wrapper carrying exactly the one subject (flat body → 400).
pub fn classroom_body(instance_id: &str, subject_id: &str, answer: &Answer) -> Value {
    json!({ "exam_paper_instance_id": instance_id, "subjects": [exam_subject_entry(subject_id, answer)] })
}

pub fn homework_body(text: &str) -> Value {
    json!({ "comment": text, "is_draft": false, "slides": [], "uploads": [] })
}

pub fn questionnaire_body(items: &[(String, Answer)]) -> Value {
    // Same wrapper shape as courseware; the questionnaire endpoint differs, not the body.
    courseware_body(items)
}

fn answer_texts(a: &Answer) -> Vec<String> {
    match a {
        Answer::Blanks(b) => b.clone(),
        Answer::Text(t) => vec![t.clone()],
        Answer::Options(ids) => ids.clone(),
        Answer::Vote(l) => l.clone(),
        Answer::Matching(p) => p.iter().map(|(_, o)| o.clone()).collect(),
    }
}

/// Submit an exam and return the submission id (for the resubmit-for-correct review read).
pub async fn submit_exam(
    client: &Client,
    ep: &Endpoints,
    activity_id: &str,
    instance_id: &str,
    answers: &std::collections::HashMap<String, Answer>,
    subjects: &[Value],
) -> Result<String, String> {
    let entries: Vec<Value> = subjects
        .iter()
        .filter_map(|s| {
            let sid = quiz::subject_id(s);
            answers.get(&sid).map(|a| exam_subject_entry(&sid, a)) // skip subjects with no answer (never blank)
        })
        .collect();
    let body = exam_body(instance_id, &entries);
    let v: Value = client
        .post(ep.exam_submissions(activity_id))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("submit: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(v.get("submission_id").and_then(Value::as_str).unwrap_or("").to_string())
}

/// Resubmit leaked-correct answers for full marks (docs 31): read the review, decide again (now with
/// leaked answers → Replay), resubmit. Returns Ok(()) even if the server 400s a duplicate (guard only).
pub async fn resubmit_correct(
    client: &Client,
    ep: &Endpoints,
    activity_id: &str,
    instance_id: &str,
    submission_id: &str,
) -> Result<(), String> {
    let review: Value = client
        .get(ep.exam_submission_review(activity_id, submission_id))
        .send()
        .await
        .map_err(|e| format!("review: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let subjects = review
        .pointer("/subjects_data/subjects")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if subjects.is_empty() {
        return Ok(());
    }
    // decide_paper over the review (leaked is_answer) → all Replay; resubmit verbatim.
    let mut answers = std::collections::HashMap::new();
    for plan in quiz::decide_paper(&subjects) {
        if let Decision::Replay(a) = plan.decision {
            answers.insert(plan.subject_id, a);
        }
    }
    let _ = submit_exam(client, ep, activity_id, instance_id, &answers, &subjects).await; // 400 on dup is fine
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exam_body_shapes_verbatim() {
        let entries = vec![
            exam_subject_entry("s1", &Answer::Options(vec!["o2".into()])),
            exam_subject_entry("s2", &Answer::Text("<p>巴黎</p>".into())),
            exam_subject_entry("s3", &Answer::Blanks(vec!["<b>1</b>".into(), "2".into()])),
        ];
        let body = exam_body("inst-1", &entries);
        assert_eq!(body["examFinished"], true);
        assert_eq!(body["exam_paper_instance_id"], "inst-1");
        assert_eq!(body["subjects"][0]["answer_option_ids"], json!(["o2"]));
        assert_eq!(body["subjects"][1]["answer"], "<p>巴黎</p>"); // verbatim, not stripped
        assert_eq!(body["subjects"][2]["answers"], json!(["<b>1</b>", "2"]));
    }

    #[test]
    fn per_source_bodies_match_contract() {
        assert_eq!(vote_body(&["A".into(), "C".into()]), json!({ "votes": ["A", "C"] }));
        assert_eq!(
            homework_body("my essay"),
            json!({ "comment": "my essay", "is_draft": false, "slides": [], "uploads": [] })
        );
        // classroom-exam: full exam wrapper with exactly the one subject.
        let cb = classroom_body("inst-9", "s5", &Answer::Text("answer".into()));
        assert_eq!(cb["exam_paper_instance_id"], "inst-9");
        assert_eq!(cb["subjects"].as_array().unwrap().len(), 1);
        assert_eq!(cb["subjects"][0]["subject_id"], "s5");
    }
}

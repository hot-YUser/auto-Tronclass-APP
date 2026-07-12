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

/// Re-ask correction (docs 31 R3b) — verbatim; sent as the user turn after threading the bad reply.
const CORRECTION_PROMPT: &str = "Your previous reply could not be used as an answer. Answer this question NOW in the exact required format: for multiple choice reply ONLY the option LETTER(S) (e.g. B or A,C) — never the option text; for fill-in give the blank text (multiple blanks separated by ' ||| '); for open questions a short direct answer. Do NOT explain, and NEVER leave it blank.";

pub struct Paper {
    pub instance_id: String,
    pub subjects: Vec<Value>,
    pub allow_retake: bool,
    pub reveal: bool,
}

/// A JSON id as a string whether the server sent it as a number or a string (ids like
/// `exam_paper_instance_id` come back as integers). Empty when absent.
fn json_id_string(v: Option<&Value>) -> String {
    v.and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
        .unwrap_or_default()
}

/// Fetch the paper for an activity, per the family's real contract (docs 31). exam/classroom/
/// questionnaire use their own `distribute` segment; courseware its subjects endpoint — all four run
/// `quiz::flatten_paper`. vote synthesizes subjects from `vote_option_items`; homework synthesizes one
/// `short_answer` from the stem. `stem` is the activity's raw description (from detection), if any.
pub async fn fetch_paper(client: &Client, ep: &Endpoints, source: Source, activity_id: &str, stem: &str) -> Result<Paper, String> {
    let url = match source {
        Source::Exam => {
            let _ = client.get(ep.exam_qualification(activity_id)).send().await; // best-effort gate
            ep.exam_distribute(activity_id)
        }
        Source::ClassroomExam => ep.classroom_distribute(activity_id),
        Source::Questionnaire => ep.questionnaire_distribute(activity_id),
        Source::CoursewareQuiz => ep.courseware_subjects(activity_id),
        Source::Vote => return fetch_vote_paper(client, ep, activity_id).await,
        Source::Homework => return Ok(homework_paper(client, ep, activity_id, stem).await),
    };
    let v: Value = client.get(url).send().await.map_err(|e| format!("distribute: {e}"))?.json().await.map_err(|e| e.to_string())?;
    let raw = v.get("subjects").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(Paper {
        // Real distribute returns this as an integer (e.g. 635061) — reading it as_str gave "" and the
        // submit then dropped the instance id. Accept int OR string (confirmed live 2026-07).
        instance_id: json_id_string(v.get("exam_paper_instance_id")),
        subjects: quiz::flatten_paper(&raw),
        allow_retake: v.get("allow_retake_exam").and_then(Value::as_bool).unwrap_or(false),
        reveal: v.get("announce_answer").and_then(Value::as_str) == Some("immediate"),
    })
}

/// vote: `GET votes/{id}` → `interaction.data.vote_option_items` (letter→text) → one selection subject
/// whose option ids ARE the letters (A=1st…); the submit then sends `{votes:[letters]}`.
async fn fetch_vote_paper(client: &Client, ep: &Endpoints, activity_id: &str) -> Result<Paper, String> {
    let v: Value = client.get(ep.votes_read(activity_id)).send().await.map_err(|e| format!("vote: {e}"))?.json().await.map_err(|e| e.to_string())?;
    let mut opts = Vec::new();
    if let Some(Value::Object(m)) = v.pointer("/interaction/data/vote_option_items") {
        let mut keys: Vec<&String> = m.keys().collect();
        keys.sort();
        for k in keys {
            opts.push(json!({ "id": k, "content": m.get(k).and_then(Value::as_str).unwrap_or("") }));
        }
    }
    // single-vs-multi: `vote_type` containing "multi" → multiple_selection, else single (caps to 1 letter).
    let multi = v.pointer("/interaction/data/vote_type").and_then(Value::as_str).map(|t| t.contains("multi")).unwrap_or(false);
    let vtype = if multi { "multiple_selection" } else { "single_selection" };
    let subject = json!({ "id": activity_id, "type": vtype, "answer_type": "vote", "content": "Vote", "options": opts });
    Ok(Paper { instance_id: String::new(), subjects: vec![subject], allow_retake: false, reveal: false })
}

/// homework: no distribute. Prefer the raw `stem`, else a **guarded** activity-detail GET (teacher-only
/// on some tenants — non-fatal), else a default → synth one `short_answer` for the LLM to write.
async fn homework_paper(client: &Client, ep: &Endpoints, activity_id: &str, stem: &str) -> Paper {
    let mut prompt = stem.trim().to_string();
    if prompt.is_empty() {
        if let Ok(resp) = client.get(ep.activity_detail(activity_id)).send().await {
            if let Ok(v) = resp.json::<Value>().await {
                prompt = v.get("description").and_then(Value::as_str)
                    .or_else(|| v.pointer("/data/description").and_then(Value::as_str))
                    .or_else(|| v.get("title").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
            }
        }
    }
    if prompt.trim().is_empty() {
        prompt = "Please write a short response for this assignment.".to_string();
    }
    let subject = json!({ "id": activity_id, "type": "short_answer", "answer_type": "short_answer", "content": prompt });
    Paper { instance_id: String::new(), subjects: vec![subject], allow_retake: false, reveal: false }
}

/// Decide every subject; for pending ones ask the LLM (streaming reasoning). Shared answers, run once
/// per activity. Blank/pending subjects re-asked up to `max_reask`; a persistent empty is dropped
/// (never submit blank).
#[allow(clippy::too_many_arguments)]
pub async fn shared_answers(
    client: &Client,
    cfg: &LlmConfig,
    cb: llm::EventCb,
    quiz_id: &str,
    course_id: &str,
    base_url: &str,
    subjects: &[Value],
    max_reask: u32,
    prior: &std::collections::HashMap<String, Answer>,
) -> std::collections::HashMap<String, Answer> {
    // Seed with answers already resolved on an earlier prepare (R3c re-prepare) — a still-missing
    // subject is re-asked; a leaked answer (Replay) still overwrites (authoritative).
    let mut answers = prior.clone();
    for plan in quiz::decide_paper(subjects) {
        match plan.decision {
            Decision::Skip => {}
            Decision::Replay(a) => {
                answers.insert(plan.subject_id, a);
            }
            Decision::Pending => {
                if answers.contains_key(&plan.subject_id) {
                    continue; // already answered on a prior pass — don't re-ask (token-thrifty)
                }
                if let Some(subject) = subjects.iter().find(|s| quiz::subject_id(s) == plan.subject_id) {
                    // R5: build the user content ONCE per subject (may fetch + base64 images) and reuse it
                    // on every re-ask — never re-download/re-encode a subject's images per attempt.
                    let user_content = build_user_content(client, base_url, subject).await;
                    let tool_ctx = (cfg.enable_tools && !course_id.is_empty())
                        .then_some(llm::ToolCtx { base_url, course_id, max_iterations: cfg.max_tool_iterations });
                    let mut last_reply = String::new();
                    for attempt in 0..max_reask.max(1) {
                        // Non-accumulating (v1): each re-ask is a fresh, bounded list carrying ONLY the
                        // most recent bad reply — a growing "wrong again" chain degrades reasoning models.
                        let messages = if attempt == 0 {
                            vec![json!({ "role": "user", "content": user_content.clone() })]
                        } else {
                            vec![
                                json!({ "role": "user", "content": user_content.clone() }),
                                json!({ "role": "assistant", "content": last_reply }),
                                json!({ "role": "user", "content": CORRECTION_PROMPT }),
                            ]
                        };
                        let reply = llm::answer_question(client, cfg, &messages, cb, quiz_id, &plan.subject_id, tool_ctx.as_ref()).await.unwrap_or_default();
                        if !reply.is_empty() {
                            if let Some(a) = parse_answer(subject, &plan.qtype, &reply) {
                                answers.insert(plan.subject_id.clone(), a);
                                break;
                            }
                        }
                        last_reply = reply;
                    }
                    // Still nothing → leave it out; the submit skips it (never blank).
                }
            }
        }
    }
    answers
}

/// The ids of every non-`Skip` subject (per `decide_paper`) absent from `answers`. Empty ⇒ the paper is
/// fully answered and ready to submit — the R3c all-or-nothing gate (never submit a half-paper).
pub fn missing_subjects(subjects: &[Value], answers: &std::collections::HashMap<String, Answer>) -> Vec<String> {
    quiz::decide_paper(subjects)
        .into_iter()
        .filter(|p| p.decision != Decision::Skip && !answers.contains_key(&p.subject_id))
        .map(|p| p.subject_id)
        .collect()
}

/// The subject's question text — `description`, else `content`, else `stem` (v1 answer_flow.py:92).
/// The real exam distribute puts the question in `description`; reading only `content` gave an EMPTY
/// stem (the LLM saw options with no question and answered nothing) — confirmed live 2026-07.
fn subject_stem(subject: &Value) -> &str {
    // v1 `description or content or stem` — a null/empty field falls through (not "present but blank").
    ["description", "content", "stem"]
        .iter()
        .find_map(|k| subject.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("")
}

/// User content (docs 31 R3b): stem + each option `<LETTER>. <content>` (letters, no ids); for
/// fill/cloze a blank-count hint. short_answer = stem only. The SYSTEM_PROMPT does the rest.
fn build_prompt(subject: &Value) -> String {
    let mut p = quiz::clean_html(subject_stem(subject));
    let qtype = subject.get("type").and_then(Value::as_str).unwrap_or("");
    if let Some(opts) = subject.get("options").and_then(Value::as_array) {
        p.push('\n');
        for (i, o) in opts.iter().enumerate() {
            let c = quiz::clean_html(o.get("content").and_then(Value::as_str).unwrap_or(""));
            p.push_str(&format!("{}. {}\n", quiz::option_label(i), c));
        }
    } else if quiz::BLANK_TYPES.contains(&qtype) {
        let n = quiz::blank_count(subject);
        if n > 1 {
            p.push_str(&format!("\n[Fill in {n} blank(s), in order, separated by ' ||| '.]"));
        } else {
            p.push_str("\n[Fill in the blank.]");
        }
    }
    p
}

/// R5 multimodal: the user message content. A plain string unless the subject's stem or an option carries
/// `<img>`, in which case a parts list `[{text}, {image_url:data-url}, …]` (dedup, cap 5) so the model can
/// see the image. Each img: a `data:` url passes through; else authed-fetch → base64; a miss → the raw url.
async fn build_user_content(client: &Client, base_url: &str, subject: &Value) -> Value {
    let text = build_prompt(subject);
    let mut html = subject_stem(subject).to_string();
    if let Some(opts) = subject.get("options").and_then(Value::as_array) {
        for o in opts {
            html.push_str(o.get("content").and_then(Value::as_str).unwrap_or(""));
        }
    }
    let srcs = extract_img_srcs(&html);
    if srcs.is_empty() {
        return Value::String(text); // no images → the plain-string path (unchanged)
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    for src in srcs.into_iter().take(5) {
        parts.push(json!({ "type": "image_url", "image_url": { "url": to_data_url(client, base_url, &src).await } }));
    }
    Value::Array(parts)
}

/// `<img src>` values from an HTML fragment (deduped, in order).
fn extract_img_srcs(html: &str) -> Vec<String> {
    let Ok(dom) = tl::parse(html, tl::ParserOptions::default()) else { return Vec::new() };
    let parser = dom.parser();
    let mut out = Vec::new();
    if let Some(imgs) = dom.query_selector("img") {
        for h in imgs {
            if let Some(src) = h.get(parser).and_then(|n| n.as_tag()).and_then(|t| t.attributes().get("src").flatten()) {
                let s = src.as_utf8_str().to_string();
                if !s.is_empty() && !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// An `<img>` src → an inline `data:` url (base64) when we can authed-fetch it; else the resolved raw url.
async fn to_data_url(client: &Client, base_url: &str, src: &str) -> String {
    if src.starts_with("data:") {
        return src.to_string();
    }
    let url = if src.starts_with("http://") || src.starts_with("https://") {
        src.to_string()
    } else {
        reqwest::Url::parse(base_url).and_then(|b| b.join(src)).map(|u| u.to_string()).unwrap_or_else(|_| src.to_string())
    };
    match crate::course_context::fetch_image(client, &url).await {
        Some((bytes, mime)) => {
            let sub = mime.strip_prefix("image/").unwrap_or("png");
            let sub = if ["png", "jpeg", "gif", "webp"].contains(&sub) { sub } else { "png" };
            format!("data:image/{sub};base64,{}", crate::login::encode_base64(&bytes))
        }
        None => url, // fetch miss → fall back to the raw url
    }
}

/// Map the LLM reply back to a concrete answer for the subject's type (delegates to the pure quiz
/// parsers). single_selection/true_or_false keep one option; multiple_selection + a degenerate
/// matching keep all (v1); blanks split on ' ||| '; short_answer verbatim.
fn parse_answer(subject: &Value, qtype: &str, text: &str) -> Option<Answer> {
    match qtype {
        "single_selection" | "true_or_false" | "multiple_selection" | "matching" => {
            let opts = subject.get("options").and_then(Value::as_array)?;
            let single = qtype == "single_selection" || qtype == "true_or_false";
            let ids = quiz::parse_choice_reply(text, opts, single);
            (!ids.is_empty()).then_some(Answer::Options(ids))
        }
        t if quiz::BLANK_TYPES.contains(&t) => {
            let blanks = quiz::parse_blanks(text, quiz::blank_count(subject));
            blanks.iter().any(|b| !b.is_empty()).then_some(Answer::Blanks(blanks))
        }
        _ => {
            let t = text.trim();
            (!t.is_empty()).then(|| Answer::Text(t.to_string()))
        }
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

/// One subject's answer as an exam-wrapper subject entry (verbatim). Adds `parent_id` for a flattened
/// matching sub; fill/cloze blanks → `answers:[{sort,content}]` (0-based sort, first pass).
pub fn exam_subject_entry(subject: &Value, answer: &Answer) -> Value {
    let mut e = json!({ "subject_id": quiz::id_value(&quiz::subject_id(subject)), "answer_option_ids": [], "answer": "" });
    match answer {
        Answer::Options(ids) => e["answer_option_ids"] = json!(quiz::id_values(ids)),
        Answer::Text(t) => e["answer"] = json!(t),
        Answer::Blanks(b) => e["answers"] = json!(blanks_sort_content(b)),
        Answer::Vote(l) => e["answer_option_ids"] = json!(l), // vote letters, not numeric ids
    }
    // parent_id (matching sub) is numeric on the real server → emit it int-or-string, and read it as such
    // (the old `as_str`-only read dropped a numeric parent_id, un-binding a flattened matching sub).
    if let Some(pid) = parent_id_str(subject) {
        e["parent_id"] = quiz::id_value(&pid);
    }
    e
}

/// A subject's `parent_id` int-or-string; `None` when absent/null (a top-level subject).
fn parent_id_str(subject: &Value) -> Option<String> {
    let s = json_id_string(subject.get("parent_id"));
    (!s.is_empty()).then_some(s)
}

/// `[{sort:i, content}]` — 0-based per-blank (first pass; the resubmit overlay preserves the review's raw sort).
fn blanks_sort_content(blanks: &[String]) -> Vec<Value> {
    blanks.iter().enumerate().map(|(i, c)| json!({ "sort": i, "content": c })).collect()
}

pub fn exam_body(instance_id: &str, entries: &[Value]) -> Value {
    json!({ "exam_paper_instance_id": quiz::id_value(instance_id), "examFinished": true, "subjects": entries })
}

/// questionnaire: the exam wrapper WITHOUT `examFinished` — NOT the courseware body.
pub fn questionnaire_body(instance_id: &str, entries: &[Value]) -> Value {
    json!({ "exam_paper_instance_id": quiz::id_value(instance_id), "subjects": entries })
}

pub fn vote_body(letters: &[String]) -> Value {
    json!({ "votes": letters })
}

/// courseware: a DISTINCT body — `subjects_answers` with `type`=answer_type and BOTH `answer_option_ids`
/// + `answers:[{sort,content}]` always present (no scalar `answer`). items = (subject_id, answer_type, answer).
pub fn courseware_body(items: &[(String, String, Answer)]) -> Value {
    let arr: Vec<Value> = items
        .iter()
        .map(|(sid, atype, a)| {
            let mut e = json!({ "subject_id": quiz::id_value(sid), "type": atype, "answer_option_ids": [], "answers": [] });
            match a {
                Answer::Options(ids) => e["answer_option_ids"] = json!(quiz::id_values(ids)),
                Answer::Vote(l) => e["answer_option_ids"] = json!(l),
                Answer::Blanks(b) => e["answers"] = json!(blanks_sort_content(b)),
                Answer::Text(t) => e["answers"] = json!([{ "sort": 0, "content": t }]),
            }
            e
        })
        .collect();
    json!({ "subjects_answers": arr })
}

/// classroom-exam: full exam wrapper carrying exactly the one subject (flat body → 400).
pub fn classroom_body(instance_id: &str, subject: &Value, answer: &Answer) -> Value {
    json!({ "exam_paper_instance_id": quiz::id_value(instance_id), "subjects": [exam_subject_entry(subject, answer)] })
}

pub fn homework_body(text: &str) -> Value {
    json!({ "comment": text, "is_draft": false, "slides": [], "uploads": [] })
}

/// Submit an exam; return `(submission_id, allow_retake_exam)` — both from the response (the retake
/// flag gates the resubmit-for-correct pass; docs 31 / v1 answer_flow.py:456).
pub async fn submit_exam(
    client: &Client,
    ep: &Endpoints,
    activity_id: &str,
    instance_id: &str,
    answers: &std::collections::HashMap<String, Answer>,
    subjects: &[Value],
) -> Result<(String, bool), String> {
    let entries: Vec<Value> = subjects
        .iter()
        .filter_map(|s| answers.get(&quiz::subject_id(s)).map(|a| exam_subject_entry(s, a))) // skip un-answered (never blank)
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
    // submission_id comes back as an INTEGER (e.g. 681504) — reading it as_str gave "" and disabled the
    // resubmit-for-correct pass; accept int OR string, with the v1 `id` fallback (confirmed live 2026-07).
    let sid = json_id_string(v.get("submission_id").or_else(|| v.get("id")));
    let retake = v.get("allow_retake_exam").and_then(Value::as_bool).unwrap_or(false);
    Ok((sid, retake))
}

/// Resubmit for full marks (docs 31 / v1 answer_flow.py:499-523): **re-distribute** for the fresh
/// `exam_paper_instance_id` (a retake mints a new one — the original 400s) and member validation, read
/// `correct_answers_data.correct_answers`, **overlay** onto the first answers where the **review value
/// wins** (option_ids member-validated → cross-block dropped; blanks with the review's raw `sort`;
/// short from the review `answer`), preserving `parent_id`, then resubmit. Guard-only (dup 400 is fine).
pub async fn resubmit_correct(
    client: &Client,
    ep: &Endpoints,
    activity_id: &str,
    submission_id: &str,
    base_answers: &std::collections::HashMap<String, Answer>,
    _base_subjects: &[Value],
) -> Result<(), String> {
    let paper = fetch_paper(client, ep, Source::Exam, activity_id, "").await?; // fresh instance + subjects
    let review: Value = client
        .get(ep.exam_submission_review(activity_id, submission_id))
        .send()
        .await
        .map_err(|e| format!("review: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let corrects: std::collections::HashMap<String, Value> = review
        .pointer("/correct_answers_data/correct_answers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|e| (quiz::subject_id(e), e.clone())).collect())
        .unwrap_or_default();

    let mut entries = Vec::new();
    for s in &paper.subjects {
        let sid = quiz::subject_id(s);
        let entry = match corrects.get(&sid) {
            Some(rc) => overlay_entry(s, rc, base_answers.get(&sid)),
            None => base_answers.get(&sid).map(|a| exam_subject_entry(s, a)),
        };
        if let Some(e) = entry {
            entries.push(e);
        }
    }
    if entries.is_empty() {
        return Ok(());
    }
    let body = exam_body(&paper.instance_id, &entries);
    let _ = client.post(ep.exam_submissions(activity_id)).json(&body).send().await; // dup 400 is fine
    Ok(())
}

/// Overlay one review-correct entry onto a fresh subject — the review value WINS; `base` is fallback.
fn overlay_entry(subject: &Value, review: &Value, base: Option<&Answer>) -> Option<Value> {
    let mut e = json!({ "subject_id": quiz::id_value(&quiz::subject_id(subject)), "answer_option_ids": [], "answer": "" });
    let mut set = false;
    if let Some(ids) = review.get("answer_option_ids").and_then(Value::as_array) {
        // The review leaks these as INTEGERS (e.g. [3915177]); reading them as_str dropped them all, so
        // the resubmit never applied the correct option and the score never improved (confirmed live).
        let valid: Vec<String> = ids
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
            .filter(|id| subject_has_option(subject, id)) // cross-block ids dropped
            .collect();
        if !valid.is_empty() {
            e["answer_option_ids"] = json!(quiz::id_values(&valid));
            set = true;
        }
    }
    if !set {
        // review blanks: keep the server's RAW sort (v1 answer_flow.py:511).
        if let Some(arr) = review.get("answers").or_else(|| review.get("correct_answers")).and_then(Value::as_array) {
            let blanks: Vec<Value> = arr
                .iter()
                .filter_map(|b| {
                    let content = b.get("content").and_then(Value::as_str)?;
                    Some(json!({ "sort": b.get("sort").and_then(Value::as_i64).unwrap_or(0), "content": content }))
                })
                .collect();
            if !blanks.is_empty() {
                e["answers"] = json!(blanks);
                set = true;
            }
        }
    }
    if !set {
        if let Some(t) = review.get("answer").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            e["answer"] = json!(t);
            set = true;
        }
    }
    if !set {
        match base? {
            Answer::Options(ids) => e["answer_option_ids"] = json!(quiz::id_values(ids)),
            Answer::Text(t) => e["answer"] = json!(t),
            Answer::Blanks(b) => e["answers"] = json!(blanks_sort_content(b)),
            Answer::Vote(l) => e["answer_option_ids"] = json!(l),
        }
    }
    if let Some(pid) = parent_id_str(subject) {
        e["parent_id"] = quiz::id_value(&pid);
    }
    Some(e)
}

fn subject_has_option(subject: &Value, id: &str) -> bool {
    subject
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter().any(|o| {
                let oid = o.get("id");
                oid.and_then(Value::as_str) == Some(id)
                    || oid.and_then(Value::as_i64).map(|n| n.to_string()).as_deref() == Some(id)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exam_body_shapes_verbatim() {
        let entries = vec![
            exam_subject_entry(&json!({"id":"s1"}), &Answer::Options(vec!["o2".into()])),
            exam_subject_entry(&json!({"id":"s2"}), &Answer::Text("<p>巴黎</p>".into())),
            exam_subject_entry(&json!({"id":"s3"}), &Answer::Blanks(vec!["<b>1</b>".into(), "2".into()])),
            exam_subject_entry(&json!({"id":"L1","parent_id":"m"}), &Answer::Options(vec!["o1".into()])),
        ];
        let body = exam_body("inst-1", &entries);
        assert_eq!(body["examFinished"], true);
        assert_eq!(body["exam_paper_instance_id"], "inst-1");
        assert_eq!(body["subjects"][0]["answer_option_ids"], json!(["o2"]));
        assert_eq!(body["subjects"][1]["answer"], "<p>巴黎</p>"); // verbatim, not stripped
        // fill/cloze: per-blank [{sort,content}] (0-based), HTML kept.
        assert_eq!(body["subjects"][2]["answers"], json!([{"sort":0,"content":"<b>1</b>"},{"sort":1,"content":"2"}]));
        // a flattened matching sub carries parent_id.
        assert_eq!(body["subjects"][3]["parent_id"], "m");
    }

    #[test]
    fn missing_subjects_gates_on_non_skip() {
        use std::collections::HashMap;
        // s1 answerable (has answer), s2 answerable (no answer → missing), p paragraph_desc (Skip → ignored).
        let subjects = vec![
            json!({"id":"s1","type":"short_answer"}),
            json!({"id":"s2","type":"short_answer"}),
            json!({"id":"p","type":"paragraph_desc"}),
        ];
        let mut answers: HashMap<String, Answer> = HashMap::new();
        answers.insert("s1".into(), Answer::Text("done".into()));
        assert_eq!(missing_subjects(&subjects, &answers), vec!["s2".to_string()]);
        answers.insert("s2".into(), Answer::Text("done".into()));
        assert!(missing_subjects(&subjects, &answers).is_empty(), "all non-Skip answered ⇒ ready");
    }

    #[test]
    fn per_source_bodies_match_contract() {
        assert_eq!(vote_body(&["A".into(), "C".into()]), json!({ "votes": ["A", "C"] }));
        assert_eq!(
            homework_body("my essay"),
            json!({ "comment": "my essay", "is_draft": false, "slides": [], "uploads": [] })
        );
        // classroom-exam: full exam wrapper with exactly the one subject.
        let cb = classroom_body("inst-9", &json!({"id":"s5"}), &Answer::Text("answer".into()));
        assert_eq!(cb["exam_paper_instance_id"], "inst-9");
        assert_eq!(cb["subjects"].as_array().unwrap().len(), 1);
        assert_eq!(cb["subjects"][0]["subject_id"], "s5");
        // courseware: subjects_answers, distinct builder (both keys present).
        let cw = courseware_body(&[("s1".into(), "short_answer".into(), Answer::Text("hi".into()))]);
        assert_eq!(cw["subjects_answers"][0]["subject_id"], "s1");
        assert_eq!(cw["subjects_answers"][0]["type"], "short_answer");
        assert_eq!(cw["subjects_answers"][0]["answers"], json!([{"sort":0,"content":"hi"}]));
        // questionnaire: exam wrapper, NO examFinished.
        let qb = questionnaire_body("inst-q", &[exam_subject_entry(&json!({"id":"q1"}), &Answer::Options(vec!["a".into()]))]);
        assert_eq!(qb["exam_paper_instance_id"], "inst-q");
        assert!(qb.get("examFinished").is_none());
        assert_eq!(qb["subjects"][0]["subject_id"], "q1");
    }
}

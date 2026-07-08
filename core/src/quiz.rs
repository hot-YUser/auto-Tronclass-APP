//! Quiz decision layer (docs 31) — **pure, zero I/O, unit-tested.** `decide_paper` looks at each
//! subject: a server-leaked correct answer → `Replay`; otherwise → `Pending` (LLM answers it later).
//! Every scored subject gets a real answer; nothing is blind-guessed.
//!
//! Scoring gotchas are handled here:
//!  1. fill/cloze/short_answer values are kept **verbatim (HTML included)** — a display-only
//!     `clean_html` never touches submit values.
//!  2. matching applies a leaked correct id only if it's genuinely a real option of that subject
//!     (member validation), else it keeps the first answer.

use serde_json::Value;

const GROUP_TYPES: [&str; 2] = ["media", "analysis"];
const SKIP_TYPES: [&str; 1] = ["paragraph_desc"];
pub const BLANK_TYPES: [&str; 2] = ["fill_in_blank", "cloze"];

/// A concrete answer for one subject. Text/Blanks are verbatim (gotcha 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Options(Vec<String>),             // answer_option_ids (selection / true_or_false)
    Blanks(Vec<String>),              // fill_in_blank / cloze — one string per blank, verbatim
    Text(String),                     // short_answer — verbatim
    Vote(Vec<String>),                // vote — option letters
    Matching(Vec<(String, String)>),  // (sub_subject_id, option_id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Replay(Answer), // server leaked the correct answer
    Pending,        // needs the LLM
    Skip,           // paragraph_desc etc.
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectPlan {
    pub subject_id: String,
    pub qtype: String,
    pub decision: Decision,
}

pub fn subject_id(s: &Value) -> String {
    s.get("subject_id")
        .or_else(|| s.get("id"))
        .and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
        .unwrap_or_default()
}

/// Decide every subject in a paper, flattening group types into their children.
pub fn decide_paper(subjects: &[Value]) -> Vec<SubjectPlan> {
    let mut out = Vec::new();
    for s in subjects {
        decide_subject(s, &mut out);
    }
    out
}

fn decide_subject(s: &Value, out: &mut Vec<SubjectPlan>) {
    let qtype = s.get("type").and_then(Value::as_str).unwrap_or("").to_string();

    if SKIP_TYPES.contains(&qtype.as_str()) {
        out.push(SubjectPlan { subject_id: subject_id(s), qtype, decision: Decision::Skip });
        return;
    }
    if GROUP_TYPES.contains(&qtype.as_str()) {
        if let Some(children) = s.get("sub_subjects").and_then(Value::as_array) {
            for child in children {
                decide_subject(child, out);
            }
        }
        return;
    }

    let decision = match leaked_answer(s, &qtype) {
        Some(ans) => Decision::Replay(ans),
        None => Decision::Pending,
    };
    out.push(SubjectPlan { subject_id: subject_id(s), qtype, decision });
}

/// Extract a leaked correct answer if the server exposed one (review pages, or a pre-filled paper).
fn leaked_answer(s: &Value, qtype: &str) -> Option<Answer> {
    match qtype {
        "single_selection" | "multiple_selection" | "true_or_false" => {
            let ids: Vec<String> = s
                .get("options")
                .and_then(Value::as_array)?
                .iter()
                .filter(|o| o.get("is_answer").and_then(Value::as_bool) == Some(true))
                .filter_map(|o| o.get("id").and_then(|x| x.as_str().map(str::to_string)))
                .collect();
            (!ids.is_empty()).then_some(Answer::Options(ids))
        }
        t if BLANK_TYPES.contains(&t) => {
            let blanks: Vec<String> = s
                .get("correct_answers")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string)) // verbatim, HTML kept
                .collect();
            (!blanks.is_empty()).then_some(Answer::Blanks(blanks))
        }
        "short_answer" => s
            .get("correct_answer")
            .and_then(Value::as_str)
            .map(|t| Answer::Text(t.to_string())), // verbatim
        "matching" => leaked_matching(s),
        _ => None,
    }
}

/// matching leak with member validation: a `(sub_subject, correct_option)` pair is only kept when
/// `correct_option` is a real option of that sub_subject (gotcha 2).
fn leaked_matching(s: &Value) -> Option<Answer> {
    let subs = s.get("sub_subjects").and_then(Value::as_array)?;
    let mut pairs = Vec::new();
    for sub in subs {
        let sub_id = subject_id(sub);
        let Some(correct) = sub.get("correct_option_id").and_then(Value::as_str) else { continue };
        let valid = sub
            .get("options")
            .and_then(Value::as_array)
            .map(|opts| opts.iter().any(|o| o.get("id").and_then(Value::as_str) == Some(correct)))
            .unwrap_or(false);
        if valid {
            pairs.push((sub_id, correct.to_string()));
        }
    }
    (!pairs.is_empty()).then_some(Answer::Matching(pairs))
}

/// Display-only HTML strip. **Never** call this on a value about to be submitted (gotcha 1).
pub fn clean_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_when_leaked_pending_otherwise() {
        let leaked = json!({"id":"s1","type":"single_selection","options":[
            {"id":"a","is_answer":false},{"id":"b","is_answer":true}]});
        let blind = json!({"id":"s2","type":"single_selection","options":[{"id":"a"},{"id":"b"}]});
        let plans = decide_paper(&[leaked, blind]);
        assert_eq!(plans[0].decision, Decision::Replay(Answer::Options(vec!["b".into()])));
        assert_eq!(plans[1].decision, Decision::Pending);
    }

    #[test]
    fn group_flattens_skip_skips_blanks_kept_verbatim() {
        let group = json!({"id":"g","type":"media","sub_subjects":[
            {"id":"c1","type":"true_or_false","options":[{"id":"t","is_answer":true},{"id":"f"}]},
            {"id":"c2","type":"short_answer"}]});
        let skip = json!({"id":"p","type":"paragraph_desc"});
        let blank = json!({"id":"fb","type":"fill_in_blank","correct_answers":["<p>Paris</p>","<b>2</b>"]});
        let plans = decide_paper(&[group, skip, blank]);
        // group flattened into c1 (replay) + c2 (pending); paragraph skipped; blank replayed verbatim.
        assert_eq!(plans.iter().map(|p| p.subject_id.as_str()).collect::<Vec<_>>(), ["c1", "c2", "p", "fb"]);
        assert_eq!(plans[0].decision, Decision::Replay(Answer::Options(vec!["t".into()])));
        assert_eq!(plans[1].decision, Decision::Pending);
        assert_eq!(plans[2].decision, Decision::Skip);
        assert_eq!(plans[3].decision, Decision::Replay(Answer::Blanks(vec!["<p>Paris</p>".into(), "<b>2</b>".into()])));
    }

    #[test]
    fn matching_member_validation_drops_bogus_ids() {
        let m = json!({"id":"m","type":"matching","sub_subjects":[
            {"id":"L1","correct_option_id":"o1","options":[{"id":"o1"},{"id":"o2"}]},
            {"id":"L2","correct_option_id":"ghost","options":[{"id":"o3"}]}]}); // ghost not an option → dropped
        let plans = decide_paper(std::slice::from_ref(&m));
        assert_eq!(plans[0].decision, Decision::Replay(Answer::Matching(vec![("L1".into(), "o1".into())])));
    }

    #[test]
    fn clean_html_is_display_only() {
        assert_eq!(clean_html("<p>巴黎</p>"), "巴黎");
    }
}

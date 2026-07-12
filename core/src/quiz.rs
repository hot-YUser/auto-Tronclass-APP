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

/// A concrete answer for one subject. Text/Blanks verbatim (gotcha 1). Matching is flattened to
/// per-left `single_selection`s upstream (`flatten_paper`) → no Matching variant here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Options(Vec<String>), // answer_option_ids (selection / true_or_false / a flattened matching sub)
    Blanks(Vec<String>),  // fill_in_blank / cloze — one string per blank, verbatim
    Text(String),         // short_answer — verbatim
    Vote(Vec<String>),    // vote — option letters
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
    pub qtype: String,             // decision/LLM type (a degenerate matching decides as a selection)
    pub answer_type: String,       // submit type (courseware carries it; degenerate matching = "matching")
    pub parent_id: Option<String>, // set for a flattened matching sub
    pub decision: Decision,
}

pub fn subject_id(s: &Value) -> String {
    s.get("subject_id")
        .or_else(|| s.get("id"))
        .and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
        .unwrap_or_default()
}

/// Serialize an id as a JSON **number** when it's a plain non-negative integer, else a string. The real
/// TronClass submit endpoints REJECT string ids with HTTP 400 (confirmed live 2026-07: correct answers
/// as strings → 400, as ints → 201); the offline fake accepted strings, hiding this. Vote letters
/// (A, B, …) are not numeric ids, so they naturally stay strings.
pub fn id_value(s: &str) -> Value {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = s.parse::<i64>() {
            return Value::from(n);
        }
    }
    Value::String(s.to_string())
}

/// `id_value` over a slice (e.g. `answer_option_ids`).
pub fn id_values(ids: &[String]) -> Vec<Value> {
    ids.iter().map(|s| id_value(s)).collect()
}

fn option_id(o: &Value) -> String {
    o.get("id")
        .and_then(|x| x.as_str().map(str::to_string).or_else(|| x.as_i64().map(|n| n.to_string())))
        .unwrap_or_default()
}

/// The type a subject is answered by: matching decides as a selection; everything else is its type.
fn decide_type(qtype: &str) -> &str {
    if qtype == "matching" { "single_selection" } else { qtype }
}

/// Flatten group/matching containers into individually-answerable leaf subjects (v1 answer_flow.py
/// :148-183). Run on every subjects-array fetch (exam/questionnaire/classroom/courseware). A matching
/// container with `sub_subjects` → one `single_selection` per left item (id-sorted), `parent_id`-tagged,
/// options = the sub's own else a consecutive id-sorted block of the container options (`k=opts/n`);
/// block `is_answer` is preserved so the choice-leak replays. Degenerate matching (no subs) → a
/// selection over its own options but keeps `answer_type="matching"`. Groups recurse; cloze without
/// subs falls through as one blank.
pub fn flatten_paper(subjects: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for s in subjects {
        flatten_subject(s, &mut out);
    }
    out
}

fn flatten_subject(s: &Value, out: &mut Vec<Value>) {
    let qtype = s.get("type").and_then(Value::as_str).unwrap_or("");
    let subs = s.get("sub_subjects").and_then(Value::as_array).filter(|a| !a.is_empty());

    if qtype == "matching" {
        if let Some(subs) = subs {
            let parent = subject_id(s);
            let mut subs_sorted: Vec<&Value> = subs.iter().collect();
            // ids are integers → sort NUMERICALLY (string sort mis-pairs 9/10, 999/1000 → silent 0).
            subs_sorted.sort_by_key(|a| subject_id(a).parse::<i64>().unwrap_or(0));
            let mut opts: Vec<Value> = s.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
            opts.sort_by_key(|o| option_id(o).parse::<i64>().unwrap_or(0));
            let n = subs_sorted.len();
            let k = if opts.is_empty() { 0 } else { opts.len() / n };
            for (i, sub) in subs_sorted.iter().enumerate() {
                let mut child = (*sub).clone();
                child["type"] = Value::from("single_selection");
                child["answer_type"] = Value::from("single_selection");
                child["parent_id"] = Value::from(parent.clone());
                let has_own = sub.get("options").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
                if !has_own && k > 0 {
                    let block: Vec<Value> = opts[i * k..((i + 1) * k).min(opts.len())].to_vec();
                    child["options"] = Value::Array(block);
                }
                out.push(child);
            }
            return;
        }
        let mut leaf = s.clone();
        leaf["answer_type"] = Value::from("matching");
        out.push(leaf);
        return;
    }

    if GROUP_TYPES.contains(&qtype) || subs.is_some() {
        if let Some(subs) = subs {
            for child in subs {
                flatten_subject(child, out);
            }
            return;
        }
    }

    let mut leaf = s.clone();
    leaf["answer_type"] = Value::from(qtype);
    out.push(leaf);
}

/// Decide every subject in an ALREADY-flattened paper.
pub fn decide_paper(subjects: &[Value]) -> Vec<SubjectPlan> {
    subjects.iter().map(decide_subject).collect()
}

fn decide_subject(s: &Value) -> SubjectPlan {
    let qtype = s.get("type").and_then(Value::as_str).unwrap_or("").to_string();
    let answer_type = s.get("answer_type").and_then(Value::as_str).unwrap_or(&qtype).to_string();
    let parent_id = s.get("parent_id").and_then(Value::as_str).map(str::to_string);
    let id = subject_id(s);
    if SKIP_TYPES.contains(&qtype.as_str()) {
        return SubjectPlan { subject_id: id, qtype, answer_type, parent_id, decision: Decision::Skip };
    }
    let decision = match leaked_answer(s, &qtype) {
        Some(a) => Decision::Replay(a),
        None => Decision::Pending,
    };
    SubjectPlan { subject_id: id, qtype, answer_type, parent_id, decision }
}

/// Server-leaked correct answer at DISTRIBUTE — choice/matching read option `is_answer` **only**
/// (never subject `answer_option_ids`, which at distribute is the user's PRIOR answer → would replay a
/// stale wrong answer and skip the LLM). fill/cloze/short read `correct_answers:[{sort,content}]`.
fn leaked_answer(s: &Value, qtype: &str) -> Option<Answer> {
    match decide_type(qtype) {
        "single_selection" | "multiple_selection" | "true_or_false" => {
            let ids: Vec<String> = s
                .get("options")
                .and_then(Value::as_array)?
                .iter()
                .filter(|o| o.get("is_answer").and_then(Value::as_bool) == Some(true))
                .map(option_id)
                .filter(|id| !id.is_empty())
                .collect();
            (!ids.is_empty()).then_some(Answer::Options(ids))
        }
        t if BLANK_TYPES.contains(&t) => {
            let blanks = correct_answers_contents(s);
            (!blanks.is_empty()).then_some(Answer::Blanks(blanks))
        }
        "short_answer" => {
            let c = correct_answers_contents(s);
            (!c.is_empty()).then_some(Answer::Text(c.concat()))
        }
        _ => None,
    }
}

/// `correct_answers:[{sort,content}]` → contents sorted by `sort`, verbatim. The real leaked shape for
/// fill/cloze/short — not a flat string array, and no singular `correct_answer` field.
pub fn correct_answers_contents(s: &Value) -> Vec<String> {
    let Some(arr) = s.get("correct_answers").and_then(Value::as_array) else { return vec![] };
    let mut items: Vec<(i64, String)> = arr
        .iter()
        .filter_map(|e| Some((e.get("sort").and_then(Value::as_i64).unwrap_or(0), e.get("content").and_then(Value::as_str)?.to_string())))
        .collect();
    items.sort_by_key(|(sort, _)| *sort);
    items.into_iter().map(|(_, c)| c).collect()
}

// ===== R3b: LLM-reply parsing (pure; answer.rs::parse_answer delegates here) =====

/// Option index → spreadsheet letter (0→A, 25→Z, 26→AA, …) for rendering; parsing handles single letters.
pub fn option_label(mut i: usize) -> String {
    let mut chars = Vec::new();
    loop {
        chars.push((b'A' + (i % 26) as u8) as char);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    chars.iter().rev().collect()
}

/// Single/multi-letter label → 0-based index (A→0, Z→25, AA→26).
fn label_to_index(label: &str) -> Option<usize> {
    let mut idx: usize = 0;
    for c in label.chars() {
        if !c.is_ascii_uppercase() {
            return None;
        }
        idx = idx * 26 + (c as usize - 'A' as usize + 1);
    }
    (idx > 0).then(|| idx - 1)
}

/// Isolated single uppercase letters (neighbours not alphabetic → prose like "and" doesn't leak).
fn isolated_letters(reply: &str) -> Vec<String> {
    let chars: Vec<char> = reply.chars().collect();
    let mut out: Vec<String> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase()
            && !(i > 0 && chars[i - 1].is_alphabetic())
            && !(i + 1 < chars.len() && chars[i + 1].is_alphabetic())
        {
            let s = c.to_string();
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// A bare compact run (`"AC"`, lowercase `"b"`) — the whole trimmed reply is a short letter run.
fn compact_letters(reply: &str) -> Vec<String> {
    let t = reply.trim();
    if t.is_empty() || t.chars().count() > 8 || !t.chars().all(|c| c.is_ascii_alphabetic()) {
        return vec![];
    }
    let mut out: Vec<String> = Vec::new();
    for c in t.chars() {
        let s = c.to_ascii_uppercase().to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn normalize(s: &str) -> String {
    clean_html(s).to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Content fallback: match the reply against option content (exact normalized, else substring).
fn match_by_content(reply: &str, options: &[Value]) -> Option<String> {
    let r = normalize(reply);
    if r.is_empty() {
        return None;
    }
    for exact in [true, false] {
        for o in options {
            let c = normalize(o.get("content").and_then(Value::as_str).unwrap_or(""));
            if c.is_empty() {
                continue;
            }
            let hit = if exact { c == r } else { r.contains(&c) || c.contains(&r) };
            if hit {
                return Some(option_id(o));
            }
        }
    }
    None
}

/// Parse an LLM reply into option IDs for a choice/matching subject. `single` keeps only the first.
pub fn parse_choice_reply(reply: &str, options: &[Value], single: bool) -> Vec<String> {
    let mut letters = isolated_letters(reply);
    if letters.is_empty() {
        letters = compact_letters(reply);
    }
    let mut ids: Vec<String> = Vec::new();
    for l in &letters {
        if let Some(id) = label_to_index(l).and_then(|idx| options.get(idx)).map(option_id) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        if let Some(id) = match_by_content(reply, options) {
            ids.push(id);
        }
    }
    if single {
        ids.truncate(1);
    }
    ids
}

/// Split a fill reply into per-blank strings, padded/truncated to `count` (verbatim, HTML kept).
/// Splits on the `|||` separator tolerant of surrounding spaces (`aa|||bb`, `aa  |||  bb`, `aa ||| bb`
/// all → two blanks); falls back to newlines when no `|||` is present.
pub fn parse_blanks(reply: &str, count: usize) -> Vec<String> {
    let count = count.max(1);
    let mut parts: Vec<String> = if reply.contains("|||") {
        reply.split("|||").map(|s| s.trim().to_string()).collect()
    } else {
        reply.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
    };
    if parts.is_empty() {
        parts.push(reply.trim().to_string());
    }
    parts.truncate(count);
    while parts.len() < count {
        parts.push(String::new());
    }
    parts
}

/// Number of blanks (v1 answer_flow.py:84-88): authoritative `answer_number`; else marker count
/// (`__blank__` / runs of ≥2 `_` / runs of ≥2 full-width `＿`); else leaked `correct_answers` count; else 1.
pub fn blank_count(subject: &Value) -> usize {
    if let Some(n) = subject.get("answer_number").and_then(Value::as_i64) {
        if n > 0 {
            return n as usize;
        }
    }
    let markers = count_blank_markers(subject.get("content").and_then(Value::as_str).unwrap_or(""));
    if markers > 0 {
        return markers;
    }
    subject.get("correct_answers").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0).max(1)
}

fn count_blank_markers(s: &str) -> usize {
    let s = s.replace("__blank__", "\u{0}"); // sentinel: each literal marker counts once
    let mut count = s.matches('\u{0}').count();
    for run_char in ['_', '＿'] {
        let mut run = 0usize;
        for c in s.chars() {
            if c == run_char {
                run += 1;
            } else {
                if run >= 2 {
                    count += 1;
                }
                run = 0;
            }
        }
        if run >= 2 {
            count += 1;
        }
    }
    count
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
    fn flatten_group_skip_and_blank_sort() {
        let group = json!({"id":"g","type":"media","sub_subjects":[
            {"id":"c1","type":"true_or_false","options":[{"id":"t","is_answer":true},{"id":"f"}]},
            {"id":"c2","type":"short_answer"}]});
        let skip = json!({"id":"p","type":"paragraph_desc"});
        // out-of-order sorts must be reordered by `sort`; contents kept verbatim (HTML).
        let blank = json!({"id":"fb","type":"fill_in_blank","correct_answers":[{"sort":1,"content":"<b>2</b>"},{"sort":0,"content":"<p>Paris</p>"}]});
        let flat = flatten_paper(&[group, skip, blank]);
        let plans = decide_paper(&flat);
        assert_eq!(plans.iter().map(|p| p.subject_id.as_str()).collect::<Vec<_>>(), ["c1", "c2", "p", "fb"]);
        assert_eq!(plans[0].decision, Decision::Replay(Answer::Options(vec!["t".into()])));
        assert_eq!(plans[1].decision, Decision::Pending);
        assert_eq!(plans[2].decision, Decision::Skip);
        assert_eq!(plans[3].decision, Decision::Replay(Answer::Blanks(vec!["<p>Paris</p>".into(), "<b>2</b>".into()])));
    }

    #[test]
    fn matching_flattens_numeric_sorted_blocks() {
        // Bug A regression: ids are integers → NUMERIC sort. Mixed digit-length would mis-pair under
        // string order ("10" < "9", "11" < "8"). Each left item gets its own 2-option id-sorted block.
        let m = json!({"id":"m","type":"matching",
            "sub_subjects":[{"id":"10"},{"id":"9"}],
            "options":[{"id":"11","is_answer":true},{"id":"8"},{"id":"10"},{"id":"9"}]});
        let flat = flatten_paper(std::slice::from_ref(&m));
        assert_eq!(flat.len(), 2);
        assert_eq!(subject_id(&flat[0]), "9"); // subs numeric-sorted → 9, 10
        assert_eq!(flat[0]["parent_id"], "m");
        assert_eq!(flat[0]["type"], "single_selection");
        assert_eq!(flat[0]["options"], json!([{"id":"8"},{"id":"9"}])); // options numeric-sorted, k=2
        assert_eq!(flat[1]["options"], json!([{"id":"10"},{"id":"11","is_answer":true}]));
        let plans = decide_paper(&flat);
        assert_eq!(plans[1].decision, Decision::Replay(Answer::Options(vec!["11".into()]))); // block leak
        assert_eq!(plans[1].parent_id.as_deref(), Some("m"));
    }

    #[test]
    fn clean_html_is_display_only() {
        assert_eq!(clean_html("<p>巴黎</p>"), "巴黎");
    }

    fn ids(v: Vec<String>) -> Vec<String> {
        v
    }

    #[test]
    fn parse_choice_letters_and_fallbacks() {
        let opts = json!([{"id":"o1","content":"cat"},{"id":"o2","content":"dog"},{"id":"o3","content":"fish"}]);
        let o = opts.as_array().unwrap();
        let s = |a: &str| a.to_string();
        // isolated letters, multi; then single-cap.
        assert_eq!(parse_choice_reply("A, C", o, false), ids(vec![s("o1"), s("o3")]));
        assert_eq!(parse_choice_reply("A, C", o, true), ids(vec![s("o1")]));
        // prose isolated letter only ("is" lowercase; B isolated) — no leak from words.
        assert_eq!(parse_choice_reply("The answer is B.", o, true), ids(vec![s("o2")]));
        // matching-style "1-A, 2-C".
        assert_eq!(parse_choice_reply("1-A, 2-C", o, false), ids(vec![s("o1"), s("o3")]));
        // compact-run fallback "AC" and lowercase "b".
        assert_eq!(parse_choice_reply("AC", o, false), ids(vec![s("o1"), s("o3")]));
        assert_eq!(parse_choice_reply("b", o, true), ids(vec![s("o2")]));
        // content fallback (no usable letter) → exact match "dog".
        assert_eq!(parse_choice_reply("dog", o, true), ids(vec![s("o2")]));
    }

    #[test]
    fn parse_blanks_split_pad_truncate() {
        assert_eq!(parse_blanks("Paris ||| France", 2), vec!["Paris".to_string(), "France".to_string()]);
        assert_eq!(parse_blanks("only", 3), vec!["only".to_string(), String::new(), String::new()]);
        assert_eq!(parse_blanks("a ||| b ||| c", 2), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(parse_blanks("<b>x</b>", 1), vec!["<b>x</b>".to_string()]); // verbatim HTML
        // `|||` tolerant of spacing (no spaces / doubled spaces) → still splits.
        assert_eq!(parse_blanks("aa|||bb", 2), vec!["aa".to_string(), "bb".to_string()]);
        assert_eq!(parse_blanks("aa  |||  bb", 2), vec!["aa".to_string(), "bb".to_string()]);
    }

    #[test]
    fn option_label_and_blank_count() {
        assert_eq!(option_label(0), "A");
        assert_eq!(option_label(25), "Z");
        assert_eq!(option_label(26), "AA");
        // blank_count: answer_number wins; else full-width/variable markers; else 1.
        assert_eq!(blank_count(&json!({"type":"fill_in_blank","answer_number":3,"content":"__"})), 3);
        assert_eq!(blank_count(&json!({"type":"cloze","content":"____ and ＿＿ and __blank__"})), 3);
        assert_eq!(blank_count(&json!({"type":"fill_in_blank","content":"no markers"})), 1);
    }
}

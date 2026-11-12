//! Quiz detection: walk an account's courses and report each activity it can answer right now.

use super::poller::course_id;
use super::{Account, MonitorMsg, QuizDetected};
use crate::answer::Source;
use crate::http;
use crate::json;
use crate::providers::Endpoints;
use crate::time;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;

/// A course joined mid-semester shows up after at most this long.
const COURSE_REFRESH: Duration = Duration::from_secs(300);

/// What one poller has found so far, so each activity is reported once per plan generation.
#[derive(Default)]
pub(super) struct QuizScanner {
    courses: Vec<String>,
    courses_at: Option<Instant>,
    scanned_at: Option<Instant>,
    seen: HashSet<String>,
    /// Votes this account has already cast; reading one again only costs a request.
    voted: HashSet<String>,
}

/// One pass over an account's courses.
pub(super) struct Scan<'a> {
    pub(super) account: &'a Account,
    pub(super) endpoints: &'a Endpoints,
    pub(super) tx: &'a UnboundedSender<MonitorMsg>,
    pub(super) generation: u64,
}

impl QuizScanner {
    pub(super) fn due(&self, every: Duration) -> bool {
        self.scanned_at.is_none_or(|at| at.elapsed() >= every)
    }

    /// Report everything again on the next scan. The course list is kept.
    pub(super) fn forget(&mut self) {
        self.seen.clear();
        self.voted.clear();
        self.scanned_at = None;
    }

    pub(super) async fn scan(
        &mut self,
        account: &Account,
        endpoints: &Endpoints,
        tx: &UnboundedSender<MonitorMsg>,
        wanted: &[String],
        generation: u64,
    ) {
        self.refresh_courses(account, endpoints).await;
        let scan = Scan {
            account,
            endpoints,
            tx,
            generation,
        };
        let now = time::now();
        for course in self.courses.clone() {
            for family in Family::ALL {
                if !wanted.is_empty() && !wanted.iter().any(|name| name == family.name()) {
                    continue;
                }
                match family {
                    Family::Vote => self.scan_votes(&scan, &course).await,
                    Family::Courseware => self.scan_courseware(&scan, &course).await,
                    Family::Listed(listing) => {
                        for activity in scan
                            .list((listing.url)(endpoints, &course), listing.key)
                            .await
                        {
                            if (listing.answerable)(&activity, now) {
                                self.report(&scan, listing.source, &course, &activity);
                            }
                        }
                    }
                }
            }
        }
        self.scanned_at = Some(Instant::now());
    }

    async fn refresh_courses(&mut self, account: &Account, endpoints: &Endpoints) {
        let fresh = self
            .courses_at
            .is_some_and(|at| at.elapsed() < COURSE_REFRESH);
        if fresh && !self.courses.is_empty() {
            return;
        }
        let Ok(listing) = get_json(&account.client, &endpoints.my_courses()).await else {
            return;
        };
        let courses: Vec<String> = json::array_at(&listing, &["courses", "items", "data"])
            .iter()
            .filter_map(course_id)
            .collect();
        if !courses.is_empty() {
            self.courses = courses;
        }
        self.courses_at = Some(Instant::now());
    }

    /// An open vote the account has not cast yet.
    async fn scan_votes(&mut self, scan: &Scan<'_>, course: &str) {
        let interactions = scan.endpoints.course_interactions(course);
        for vote in scan.list(interactions, "interactions").await {
            if json::text_at(&vote, &["type"]) != "vote"
                || json::text_at(&vote, &["status"]) != "start"
            {
                continue;
            }
            let Some(id) = activity_id(&vote) else {
                continue;
            };
            if self.voted.contains(&id) || self.seen.contains(&seen_key(Source::Vote, course, &id))
            {
                continue;
            }
            let cast = get_json(&scan.account.client, &scan.endpoints.votes_read(&id))
                .await
                .is_ok_and(|vote| has_voted(&vote, &scan.account.user_no));
            if cast {
                self.voted.insert(id);
            } else {
                self.report(scan, Source::Vote, course, &vote);
            }
        }
    }

    /// Quizzes embedded in course material that the account has not submitted yet.
    async fn scan_courseware(&mut self, scan: &Scan<'_>, course: &str) {
        let activities = scan.endpoints.course_activities(course);
        for material in scan.list(activities, "activities").await {
            if json::text_at(&material, &["type"]) != "material" {
                continue;
            }
            let Some(material_id) = activity_id(&material) else {
                continue;
            };
            let quizzes = scan.endpoints.courseware_quizzes(&material_id);
            for quiz in scan.list(quizzes, "quizzes").await {
                if json::flag(&quiz, "is_closed", false) || !json::flag(&quiz, "is_started", true) {
                    continue;
                }
                let Some(id) = activity_id(&quiz) else {
                    continue;
                };
                if self
                    .seen
                    .contains(&seen_key(Source::CoursewareQuiz, course, &id))
                {
                    continue;
                }
                let submission = scan.endpoints.courseware_my_submission(&id);
                let submitted = get_json(&scan.account.client, &submission)
                    .await
                    .is_ok_and(|mine| mine.as_object().is_some_and(|fields| !fields.is_empty()));
                if !submitted {
                    self.report(scan, Source::CoursewareQuiz, course, &quiz);
                }
            }
        }
    }

    /// Tell the actor about an answerable activity, once.
    pub(super) fn report(
        &mut self,
        scan: &Scan<'_>,
        source: Source,
        course: &str,
        activity: &Value,
    ) {
        let Some(id) = activity_id(activity) else {
            return;
        };
        if !self.seen.insert(seen_key(source, course, &id)) {
            return;
        }
        let stem = match source {
            Source::Homework => json::text_at(activity, &["description"]),
            _ => "",
        };
        scan.tx
            .send(MonitorMsg::QuizDetected(QuizDetected {
                generation: scan.generation,
                account_id: scan.account.id.clone(),
                base_url: scan.account.base_url.clone(),
                source,
                course: json::text_at(activity, &["course_name"]).to_string(),
                course_id: course.to_string(),
                activity_id: id,
                stem: stem.to_string(),
            }))
            .ok();
    }
}

impl Scan<'_> {
    /// The items of a listing endpoint; empty when the request fails.
    async fn list(&self, url: String, key: &str) -> Vec<Value> {
        get_json(&self.account.client, &url)
            .await
            .map(|listing| json::array_at(&listing, &[key]).to_vec())
            .unwrap_or_default()
    }
}

async fn get_json(client: &Client, url: &str) -> Result<Value, String> {
    http::json_checked(client.get(url), "fetch activity").await
}

/// The families of answerable activity, in scan order. The names are the `autoanswer_types`
/// setting's vocabulary.
#[derive(Clone, Copy)]
enum Family {
    Listed(&'static Listing),
    Vote,
    Courseware,
}

/// A family whose answerable activities can be read straight off one listing.
struct Listing {
    name: &'static str,
    source: Source,
    url: fn(&Endpoints, &str) -> String,
    key: &'static str,
    answerable: fn(&Value, i64) -> bool,
}

impl Family {
    const ALL: [Family; 6] = [
        Family::Listed(&Listing {
            name: "exam",
            source: Source::Exam,
            url: Endpoints::course_exam_list,
            key: "exams",
            answerable: exam_answerable,
        }),
        Family::Listed(&Listing {
            name: "questionnaire",
            source: Source::Questionnaire,
            url: Endpoints::course_questionnaire_list,
            key: "questionnaires",
            answerable: questionnaire_answerable,
        }),
        Family::Listed(&Listing {
            name: "homework",
            source: Source::Homework,
            url: Endpoints::course_homework,
            key: "homework_activities",
            answerable: homework_answerable,
        }),
        Family::Vote,
        Family::Listed(&Listing {
            name: "classroom",
            source: Source::ClassroomExam,
            url: Endpoints::course_classroom_list,
            key: "classrooms",
            answerable: classroom_answerable,
        }),
        Family::Courseware,
    ];

    fn name(self) -> &'static str {
        match self {
            Family::Listed(listing) => listing.name,
            Family::Vote => "vote",
            Family::Courseware => "courseware",
        }
    }
}

/// The key an activity is remembered by. Ids repeat across families, so the family is part of it.
pub(super) fn seen_key(source: Source, course: &str, id: &str) -> String {
    format!("{}/{course}/{id}", source.as_str())
}

/// The id of a listed activity, whichever of the family's id fields carries it.
fn activity_id(activity: &Value) -> Option<String> {
    json::id_at(activity, &["id", "activity_id", "course_id"])
}

fn has_voted(vote: &Value, user_no: &str) -> bool {
    json::array_at(vote, &["students"]).iter().any(|student| {
        student
            .get("user_no")
            .and_then(Value::as_str)
            .is_some_and(|voter| voter.eq_ignore_ascii_case(user_no))
    })
}

/// Tenants name the submitted flag three different ways.
fn already_submitted(activity: &Value) -> bool {
    ["has_submitted", "submitted", "is_submitted"]
        .iter()
        .any(|key| json::flag(activity, key, false))
}

/// An exam is answerable once started and while open, in progress, unsubmitted, and with an
/// attempt left. A missing `is_started` means not started.
pub(super) fn exam_answerable(exam: &Value, now: i64) -> bool {
    let ended = match TimeField::read(exam, "end_time") {
        TimeField::Absent => false,
        TimeField::At(end) => end < now,
        TimeField::Invalid => return false,
    };
    let allowed = exam
        .get("submit_times")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let used = exam
        .get("submission_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    json::flag(exam, "is_started", false)
        && !json::flag(exam, "is_closed", false)
        && json::flag(exam, "is_in_progress", true)
        && !ended
        && !already_submitted(exam)
        && !(allowed > 0 && used >= allowed)
}

fn questionnaire_answerable(questionnaire: &Value, _now: i64) -> bool {
    json::flag(questionnaire, "is_started", false)
        && !json::flag(questionnaire, "is_closed", false)
        && !already_submitted(questionnaire)
}

/// Homework is skipped only on an explicit signal: not started, closed, submitted, or outside
/// its window. Missing fields never close it.
pub(super) fn homework_answerable(homework: &Value, now: i64) -> bool {
    let started = match TimeField::read(homework, "start_time") {
        TimeField::Absent => true,
        TimeField::At(start) => now >= start,
        TimeField::Invalid => false,
    };
    let open = match TimeField::read(homework, "end_time") {
        TimeField::Absent => true,
        TimeField::At(end) => now < end,
        TimeField::Invalid => false,
    };
    json::flag(homework, "is_started", true)
        && !json::flag(homework, "is_closed", false)
        && !already_submitted(homework)
        && started
        && open
}

/// A classroom exam keeps `status: "start"` after the teacher stops collecting answers; only the
/// count of started subjects drops to zero.
fn classroom_answerable(classroom: &Value, _now: i64) -> bool {
    json::text_at(classroom, &["status"]) == "start"
        && classroom
            .get("started_subjects_count")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            >= 1
}

/// A timestamp field as tenants send it: an epoch number or ISO-8601 text. A field that is present
/// but unreadable stays distinct from a missing one, so callers can fail closed on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TimeField {
    Absent,
    At(i64),
    Invalid,
}

impl TimeField {
    pub(super) fn read(activity: &Value, key: &str) -> Self {
        match activity.get(key) {
            // Tenants also send an omitted deadline as null or blank text.
            None | Some(Value::Null) => Self::Absent,
            Some(Value::String(text)) if text.trim().is_empty() => Self::Absent,
            Some(value) => value
                .as_i64()
                .or_else(|| value.as_str().and_then(time::parse_iso8601))
                .map_or(Self::Invalid, Self::At),
        }
    }
}

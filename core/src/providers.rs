//! School registry (docs 40). Logic vs data are separated: **no school literal lives in this
//! `.rs` file** — the factory seed is a bundled JSON data file, and at runtime the source of
//! truth is a copy in the user's data dir. A school is just a `base_url`; every endpoint derives
//! from it. The seed ships empty; the user adds their own school in-UI or types a raw base_url.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct School {
    pub key: String,
    pub label: String,
    pub base_url: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub default_key: Option<String>,
    #[serde(default)]
    pub schools: Vec<School>,
}

// Data, not code — keeps school names out of the binary's source (docs 40 / 90 §2).
const FACTORY_SEED: &str = include_str!("assets/providers.seed.json");

impl Registry {
    pub fn factory() -> Registry {
        serde_json::from_str(FACTORY_SEED).expect("valid factory seed")
    }

    /// Load the user's registry, seeding it from the factory on first run. Deleting the file
    /// re-seeds it (docs 40: the on-disk copy is the single source of truth once written).
    pub fn load_or_seed(path: &Path) -> Registry {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(reg) = serde_json::from_slice::<Registry>(&bytes) {
                return reg;
            }
        }
        let reg = Registry::factory();
        let _ = fs::write(path, serde_json::to_vec_pretty(&reg).unwrap_or_default());
        reg
    }

    /// Resolve an account's `school_ref` to a base_url: a raw URL passes through; otherwise it's
    /// matched against a school's key or aliases (case-insensitive).
    pub fn resolve(&self, school_ref: &str) -> Option<String> {
        if school_ref.starts_with("http://") || school_ref.starts_with("https://") {
            return Some(school_ref.to_string());
        }
        let needle = school_ref.trim().to_lowercase();
        self.schools
            .iter()
            .find(|s| {
                s.key.to_lowercase() == needle || s.aliases.iter().any(|a| a.to_lowercase() == needle)
            })
            .map(|s| s.base_url.clone())
    }
}

/// Endpoints derived from a single base_url (docs 40) — no per-school logic anywhere.
pub struct Endpoints {
    base: String,
}

impl Endpoints {
    pub fn derive(base_url: &str) -> Endpoints {
        Endpoints { base: base_url.trim_end_matches('/').to_string() }
    }
    pub fn base(&self) -> &str {
        &self.base
    }
    pub fn login_page(&self) -> String {
        format!("{}/login", self.base)
    }
    pub fn current_semester(&self) -> String {
        format!("{}/api/current-semester-info", self.base)
    }
    pub fn rollcalls(&self) -> String {
        format!("{}/api/radar/rollcalls?api_version=1.1.0", self.base)
    }

    // --- Quiz detection: per-account × per-course fan-out (docs 31; NOT a global list) ---
    pub fn my_courses(&self) -> String {
        format!("{}/api/my-courses", self.base)
    }
    pub fn course_activities(&self, cid: &str) -> String {
        format!("{}/api/courses/{cid}/activities?page=1&page_size=200", self.base)
    }
    pub fn course_exams(&self, cid: &str) -> String {
        format!("{}/api/courses/{cid}/exams", self.base)
    }
    pub fn course_homework(&self, cid: &str) -> String {
        format!("{}/api/courses/{cid}/homework-activities", self.base)
    }

    // --- exam (docs 31) ---
    pub fn exam_qualification(&self, id: &str) -> String {
        format!("{}/api/exam/{id}/check-exam-qualification", self.base)
    }
    pub fn exam_distribute(&self, id: &str) -> String {
        format!("{}/api/exams/{id}/distribute", self.base)
    }
    pub fn exam_submissions(&self, eid: &str) -> String {
        format!("{}/api/exams/{eid}/submissions", self.base)
    }
    pub fn exam_submission_review(&self, eid: &str, sid: &str) -> String {
        format!("{}/api/exams/{eid}/submissions/{sid}", self.base)
    }

    // --- vote / courseware-quiz / classroom-exam / homework / questionnaire ---
    pub fn vote_cast(&self, id: &str) -> String {
        format!("{}/api/votes/{id}/vote", self.base)
    }
    pub fn courseware_subjects(&self, id: &str) -> String {
        format!("{}/api/courseware-quiz/quiz/{id}/subjects", self.base)
    }
    pub fn courseware_submissions(&self, id: &str) -> String {
        format!("{}/api/courseware-quiz/quiz/{id}/submissions", self.base)
    }
    pub fn classroom_submit(&self, activity_id: &str, subject_id: &str) -> String {
        format!("{}/api/classroom/{activity_id}/submit/{subject_id}", self.base)
    }
    pub fn homework_submissions(&self, activity_id: &str) -> String {
        format!("{}/api/course/activities/{activity_id}/submissions", self.base)
    }
    pub fn questionnaire_submissions(&self, activity_id: &str) -> String {
        format!("{}/api/questionnaire/{activity_id}/submissions", self.base)
    }

    // --- Student answer endpoints (one per rollcall type) ---
    pub fn answer_number(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/answer_number_rollcall", self.base)
    }
    pub fn answer_radar(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/answer", self.base)
    }
    pub fn answer_self_registration(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/answer_self_registration_rollcall", self.base)
    }
    pub fn answer_qr(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/answer_qr_rollcall", self.base)
    }

    // --- Reads: roster/code/on_call_fine, attendance summary, radar-lite ---
    pub fn student_rollcalls(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/student_rollcalls", self.base)
    }
    pub fn answers(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/answers", self.base)
    }
    pub fn lite(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/lite", self.base)
    }

    // --- Teacher endpoints (QR teacher-assist; student accounts get 403) ---
    pub fn teacher_create_rollcall(&self, course_id: &str) -> String {
        format!("{}/api/course/{course_id}/rollcall", self.base)
    }
    pub fn teacher_start_rollcall(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/start-rollcall", self.base)
    }
    pub fn teacher_qr_code(&self, course_id: &str, id: &str) -> String {
        format!("{}/api/course/{course_id}/rollcall/{id}/qr_code", self.base)
    }
    pub fn teacher_stop_qr(&self, id: &str) -> String {
        format!("{}/api/rollcall/{id}/stop_qr_rollcall", self.base)
    }
}

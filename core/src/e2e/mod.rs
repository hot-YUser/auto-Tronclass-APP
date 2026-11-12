//! End-to-end tests: the core driven through its FFI entry points against the in-repo fake
//! school. `live` talks to a real school instead and only runs when asked to.

mod answer_submission;
mod course_materials;
mod detection_and_recovery;
mod live;
mod llm_answering;
mod login;
mod monitoring;
mod number_rollcall;
mod quiz_flow;
mod radar_rollcall;
mod settings_and_captcha;
mod storage;
pub(crate) mod support;
mod teacher_qr;

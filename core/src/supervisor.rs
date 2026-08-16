use crate::config::{
    AccountGroup, Config, DetectorSelection, ManualOverride, PlatformBlock, ScheduleBinding,
    TargetId,
};
use crate::protocol::{
    AccountResult, AccountResultPhase, AccountRole, AccountSnapshot, CourseSnapshot,
    DetectorSnapshot, GroupDefinitionSnapshot, LoginState, ManualOverrideSnapshot, MergeCoverage,
    MergePrompt, MonitoringSnapshot, Notice, ScheduleClockEntry, SessionState, TargetRef,
    TargetRuntimeState, TargetSnapshot, WakeMode, WireError,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SNAPSHOT_SCHEMA_VERSION: u8 = 1;
const CLOCK_GRACE_SECONDS: i64 = 120;

#[derive(Clone, Debug)]
pub struct AppliedClock {
    pub revision: u64,
    config_revision: u64,
    schedule_revision: u64,
    received_at: Instant,
    entries: HashMap<TargetId, ScheduleClockEntry>,
}

#[derive(Clone, Debug)]
pub struct LoginRuntime {
    pub state: LoginState,
    pub error: Option<WireError>,
}

impl Default for LoginRuntime {
    fn default() -> Self {
        Self {
            state: LoginState::Stored,
            error: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveRoute {
    pub source_targets: Vec<TargetId>,
    pub detector_account_id: String,
    pub participant_account_ids: Vec<String>,
    pub course_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectivePlan {
    pub revision: u64,
    pub routes: Vec<EffectiveRoute>,
    pub suppressed_accounts: HashSet<String>,
}

#[derive(Clone, Debug)]
struct ProcessOverride {
    force_open: bool,
    schedule_revision: u64,
}

#[derive(Clone, Debug)]
struct EffectiveTargetState {
    schedule_open: bool,
    effective_open: bool,
    next_boundary_utc: Option<String>,
    manual_override: Option<ManualOverrideSnapshot>,
    clock_error: Option<String>,
}

pub struct TargetSupervisor {
    clock: Option<AppliedClock>,
    process_overrides: HashMap<TargetId, ProcessOverride>,
    login: HashMap<String, LoginRuntime>,
    terminal_accounts: HashSet<String>,
    actual_detectors: HashMap<String, DetectorSnapshot>,
    acknowledged_merges: HashSet<String>,
    account_courses: HashMap<String, Vec<CourseSnapshot>>,
    account_results: HashMap<(TargetId, String), AccountResult>,
    plan: EffectivePlan,
    plan_revision: u64,
    config_notice: Option<Notice>,
    wake_mode: WakeMode,
}

impl TargetSupervisor {
    pub fn new(config_notice: Option<Notice>) -> Self {
        Self {
            clock: None,
            process_overrides: HashMap::new(),
            login: HashMap::new(),
            terminal_accounts: HashSet::new(),
            actual_detectors: HashMap::new(),
            acknowledged_merges: HashSet::new(),
            account_courses: HashMap::new(),
            account_results: HashMap::new(),
            plan: EffectivePlan::default(),
            plan_revision: 0,
            config_notice,
            wake_mode: if cfg!(target_os = "android") {
                WakeMode::Unavailable
            } else {
                WakeMode::ForegroundOnly
            },
        }
    }

    pub fn plan(&self) -> &EffectivePlan {
        &self.plan
    }

    pub fn clock_revision(&self) -> Option<u64> {
        self.clock.as_ref().map(|clock| clock.revision)
    }

    pub fn next_clock_deadline_epoch(&self) -> Option<i64> {
        self.clock.as_ref().and_then(|clock| {
            clock
                .entries
                .values()
                .filter_map(|entry| {
                    entry
                        .next_boundary_utc
                        .as_deref()
                        .and_then(parse_rfc3339_utc)
                })
                .min()
                .map(|boundary| boundary + CLOCK_GRACE_SECONDS)
        })
    }

    pub fn set_wake_mode(&mut self, mode: WakeMode) {
        if self.wake_mode != mode {
            self.wake_mode = mode;
            self.bump_plan_revision();
        }
    }

    pub fn set_login_state(
        &mut self,
        account_id: impl Into<String>,
        state: LoginState,
        error: Option<WireError>,
    ) {
        let account_id = account_id.into();
        if matches!(state, LoginState::Online | LoginState::LoggingIn) {
            self.terminal_accounts.remove(&account_id);
        }
        if state == LoginState::Error {
            self.terminal_accounts.insert(account_id.clone());
        }
        self.login.insert(account_id, LoginRuntime { state, error });
        self.bump_plan_revision();
    }

    pub fn account_is_terminal(&self, account_id: &str) -> bool {
        self.terminal_accounts.contains(account_id)
    }

    pub fn set_account_courses(&mut self, account_id: String, courses: Vec<CourseSnapshot>) {
        self.account_courses.insert(account_id, courses);
        self.bump_plan_revision();
    }

    pub fn set_account_result(
        &mut self,
        sources: &[TargetId],
        account_id: &str,
        phase: AccountResultPhase,
        activity_kind: Option<String>,
        course_name: Option<String>,
        error: Option<WireError>,
    ) {
        let updated_at_utc = Some(format_rfc3339_utc(now_epoch_seconds()));
        for source in sources {
            self.account_results.insert(
                (source.clone(), account_id.to_string()),
                AccountResult {
                    account_id: account_id.to_string(),
                    phase: phase.clone(),
                    activity_kind: activity_kind.clone(),
                    course_name: course_name.clone(),
                    updated_at_utc: updated_at_utc.clone(),
                    error: error.clone(),
                },
            );
        }
        self.bump_plan_revision();
    }

    pub fn apply_clock(
        &mut self,
        config: &Config,
        clock_revision: u64,
        config_revision: u64,
        schedule_revision: u64,
        evaluated_at_utc: &str,
        targets: Vec<ScheduleClockEntry>,
    ) -> Result<(), String> {
        if config_revision != config.config_revision
            || schedule_revision != config.schedule_revision
        {
            return Err("schedule_clock_revision_mismatch".to_string());
        }
        if self
            .clock
            .as_ref()
            .is_some_and(|clock| clock_revision <= clock.revision)
        {
            return Err("schedule_clock_not_monotonic".to_string());
        }
        let evaluated_at_epoch = parse_rfc3339_utc(evaluated_at_utc)
            .ok_or_else(|| "schedule_clock_invalid_timestamp".to_string())?;
        if (now_epoch_seconds() - evaluated_at_epoch).abs() > CLOCK_GRACE_SECONDS {
            return Err("schedule_clock_stale".to_string());
        }

        let expected: HashSet<TargetId> = schedulable_targets(config).into_iter().collect();
        let mut entries = HashMap::with_capacity(targets.len());
        for entry in targets {
            validate_clock_entry(config, &entry, evaluated_at_epoch)?;
            if !expected.contains(&entry.target)
                || entries.insert(entry.target.clone(), entry).is_some()
            {
                return Err("schedule_clock_target_mismatch".to_string());
            }
        }
        if entries.len() != expected.len() {
            return Err("schedule_clock_target_mismatch".to_string());
        }

        self.clock = Some(AppliedClock {
            revision: clock_revision,
            config_revision,
            schedule_revision,
            received_at: Instant::now(),
            entries,
        });
        self.bump_plan_revision();
        Ok(())
    }

    pub fn invalidate_clock(&mut self) {
        self.clock = None;
        self.bump_plan_revision();
    }

    /// Applies an immediate target override. `Ok(true)` means the caller must persist `config`.
    pub fn set_target_override(
        &mut self,
        config: &mut Config,
        target: &TargetId,
        force_open: bool,
    ) -> Result<bool, String> {
        ensure_target_exists(config, target)?;
        let entry = self.matching_clock_entry(config, target)?;
        if let Some(expires_at_utc) = entry.next_boundary_utc.clone() {
            config
                .runtime
                .manual_overrides
                .retain(|manual| &manual.target != target);
            config.runtime.manual_overrides.push(ManualOverride {
                target: target.clone(),
                force_open,
                expires_at_utc,
                schedule_revision: config.schedule_revision,
            });
            self.process_overrides.remove(target);
            self.bump_plan_revision();
            Ok(true)
        } else {
            self.process_overrides.insert(
                target.clone(),
                ProcessOverride {
                    force_open,
                    schedule_revision: config.schedule_revision,
                },
            );
            config
                .runtime
                .manual_overrides
                .retain(|manual| &manual.target != target);
            self.bump_plan_revision();
            Ok(false)
        }
    }

    pub fn resume_schedules(&mut self, config: &mut Config) {
        config.monitoring.all_suspended = false;
        config.runtime.manual_overrides.clear();
        self.process_overrides.clear();
        self.bump_plan_revision();
    }

    pub fn stop_all(&mut self, config: &mut Config) {
        config.monitoring.all_suspended = true;
        self.bump_plan_revision();
    }

    pub fn suspend_for_platform_limit(&mut self, config: &mut Config, reason: String) {
        config.runtime.platform_block = Some(PlatformBlock {
            reason,
            observed_at_utc: format_rfc3339_utc(now_epoch_seconds()),
        });
        self.bump_plan_revision();
    }

    pub fn clear_platform_limit(
        &mut self,
        config: &mut Config,
        reason: &str,
    ) -> Result<(), String> {
        match &config.runtime.platform_block {
            Some(block) if block.reason == reason => {
                config.runtime.platform_block = None;
                self.bump_plan_revision();
                Ok(())
            }
            Some(_) => Err("platform_block_reason_mismatch".to_string()),
            None => Ok(()),
        }
    }

    pub fn acknowledge_merge(
        &mut self,
        component_id: &str,
        plan_revision: u64,
    ) -> Result<(), String> {
        if plan_revision != self.plan_revision {
            return Err("plan_revision_conflict".to_string());
        }
        self.acknowledged_merges.insert(component_id.to_string());
        self.bump_plan_revision();
        Ok(())
    }

    /// Reconciles clock, persisted runtime and group detector rotation into one immutable plan.
    /// Returns true when a runtime cursor changed and config must be persisted.
    pub fn reconcile(&mut self, config: &mut Config) -> bool {
        let now = now_epoch_seconds();
        let before_override_count = config.runtime.manual_overrides.len();
        config.runtime.manual_overrides.retain(|manual| {
            manual.schedule_revision == config.schedule_revision
                && parse_rfc3339_utc(&manual.expires_at_utc).is_some_and(|expiry| expiry > now)
        });
        self.process_overrides
            .retain(|_, manual| manual.schedule_revision == config.schedule_revision);

        let mut runtime_changed = before_override_count != config.runtime.manual_overrides.len();
        let mut active_groups = Vec::<(AccountGroup, EffectiveTargetState)>::new();
        if !config.monitoring.all_suspended && config.runtime.platform_block.is_none() {
            for group in &config.groups {
                let target = TargetId::group(group.id.clone());
                let state = self.effective_target_state(config, &target, now);
                if state.effective_open {
                    active_groups.push((group.clone(), state));
                }
            }
        }

        let mut suppressed = HashSet::new();
        for (group, _) in &active_groups {
            suppressed.extend(group.member_account_ids.iter().cloned());
        }

        let mut routes = Vec::new();
        self.actual_detectors
            .retain(|group_id, _| active_groups.iter().any(|(group, _)| &group.id == group_id));
        for (group, state) in &active_groups {
            let (detector, fallback, changed) = self.select_detector(config, group, state);
            runtime_changed |= changed;
            if let Some(detector) = detector {
                self.actual_detectors.insert(
                    group.id.clone(),
                    DetectorSnapshot {
                        account_id: detector.clone(),
                        is_fallback: fallback,
                    },
                );
                routes.push(EffectiveRoute {
                    source_targets: vec![TargetId::group(group.id.clone())],
                    detector_account_id: detector,
                    participant_account_ids: group.member_account_ids.clone(),
                    course_ids: group.course_ids.clone(),
                });
            }
        }

        if !config.monitoring.all_suspended && config.runtime.platform_block.is_none() {
            for account in config.accounts.iter().filter(|account| !account.is_teacher) {
                if suppressed.contains(&account.id) {
                    continue;
                }
                let target = TargetId::account(account.id.clone());
                if self
                    .effective_target_state(config, &target, now)
                    .effective_open
                {
                    routes.push(EffectiveRoute {
                        source_targets: vec![target],
                        detector_account_id: account.id.clone(),
                        participant_account_ids: vec![account.id.clone()],
                        course_ids: Vec::new(),
                    });
                }
            }
        }

        let prompts = self.merge_prompts_for(config, &active_groups, &routes);
        coalesce_acknowledged(&mut routes, &prompts, &self.acknowledged_merges, config);
        routes.sort_by(|left, right| {
            left.detector_account_id
                .cmp(&right.detector_account_id)
                .then_with(|| {
                    left.source_targets[0]
                        .stable_key()
                        .cmp(&right.source_targets[0].stable_key())
                })
        });
        if self.plan.routes != routes || self.plan.suppressed_accounts != suppressed {
            self.bump_plan_revision();
            self.plan = EffectivePlan {
                revision: self.plan_revision,
                routes,
                suppressed_accounts: suppressed,
            };
        } else {
            self.plan.revision = self.plan_revision;
        }
        runtime_changed
    }

    pub fn snapshot(
        &self,
        config: &Config,
        login_in_flight: &HashSet<String>,
    ) -> MonitoringSnapshot {
        let now = now_epoch_seconds();
        let mut in_use = HashMap::<String, Vec<TargetRef>>::new();
        for route in &self.plan.routes {
            for account_id in route
                .participant_account_ids
                .iter()
                .chain(std::iter::once(&route.detector_account_id))
            {
                let refs = in_use.entry(account_id.clone()).or_default();
                for target in &route.source_targets {
                    if !refs.iter().any(|reference| &reference.target == target) {
                        refs.push(TargetRef {
                            target: target.clone(),
                            name: target_name(config, target),
                        });
                    }
                }
            }
        }

        let accounts = config
            .accounts
            .iter()
            .map(|account| {
                let login = self.login.get(&account.id).cloned().unwrap_or_default();
                AccountSnapshot {
                    account_id: account.id.clone(),
                    label: account.label.clone(),
                    school_ref: account.school_ref.clone(),
                    username: account.username.clone(),
                    role: if account.is_teacher {
                        AccountRole::Teacher
                    } else {
                        AccountRole::Student
                    },
                    teacher_course_id: account.course_id.clone(),
                    login_state: if login_in_flight.contains(&account.id) {
                        LoginState::LoggingIn
                    } else {
                        login.state
                    },
                    login_error: login.error,
                    login_in_flight: login_in_flight.contains(&account.id),
                    in_use_targets: in_use.remove(&account.id).unwrap_or_default(),
                }
            })
            .collect();

        let active_groups: Vec<_> = config
            .groups
            .iter()
            .filter_map(|group| {
                let state =
                    self.effective_target_state(config, &TargetId::group(group.id.clone()), now);
                state.effective_open.then_some((group.clone(), state))
            })
            .collect();
        let merge_prompts = self.merge_prompts_for(config, &active_groups, &self.plan.routes);
        let targets: Vec<_> = schedulable_targets(config)
            .into_iter()
            .map(|target| self.target_snapshot(config, target, now))
            .collect();
        let any_running = targets.iter().any(|target| {
            matches!(
                target.runtime_state,
                TargetRuntimeState::Starting
                    | TargetRuntimeState::Monitoring
                    | TargetRuntimeState::Stopping
            )
        });
        let no_students = !config.accounts.iter().any(|account| !account.is_teacher);
        MonitoringSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            config_revision: config.config_revision,
            schedule_revision: config.schedule_revision,
            plan_revision: self.plan_revision,
            clock_revision: self.clock.as_ref().map(|clock| clock.revision),
            session_state: if config.runtime.platform_block.is_some() {
                SessionState::PlatformBlocked
            } else if any_running {
                SessionState::Running
            } else {
                SessionState::Idle
            },
            all_suspended: config.monitoring.all_suspended,
            platform_block: config.runtime.platform_block.clone(),
            can_stop_all: !config.monitoring.all_suspended && any_running,
            can_resume: config.monitoring.all_suspended,
            global_disabled_reason: if no_students {
                Some("沒有可監控的學生帳號".to_string())
            } else {
                config
                    .runtime
                    .platform_block
                    .as_ref()
                    .map(|block| format!("平台已暫停監控：{}", block.reason))
            },
            global_schedule: config.monitoring.global_schedule.clone(),
            time_zone: config.monitoring.time_zone.clone(),
            wake_mode: self.wake_mode.clone(),
            accounts,
            targets,
            merge_prompts,
            config_notice: self.config_notice.clone(),
        }
    }

    fn target_snapshot(&self, config: &Config, target: TargetId, now: i64) -> TargetSnapshot {
        let state = self.effective_target_state(config, &target, now);
        let suppressed = match &target {
            TargetId::Account { account_id } => self.plan.suppressed_accounts.contains(account_id),
            TargetId::Group { .. } => false,
        };
        let all_suspended = config.monitoring.all_suspended;
        let platform_blocked = config.runtime.platform_block.is_some();
        let running = self
            .plan
            .routes
            .iter()
            .any(|route| route.source_targets.contains(&target));
        let runtime_state = if platform_blocked {
            TargetRuntimeState::PlatformBlocked
        } else if suppressed {
            TargetRuntimeState::SuppressedByGroup
        } else if running {
            TargetRuntimeState::Monitoring
        } else if state
            .manual_override
            .as_ref()
            .is_some_and(|manual| !manual.force_open)
        {
            TargetRuntimeState::ManualOff
        } else {
            TargetRuntimeState::ScheduledOff
        };
        let (schedule, detector, group_definition, courses, in_use_account_ids) = match &target {
            TargetId::Account { account_id } => {
                let account = config
                    .account(account_id)
                    .expect("schedulable target account exists");
                (
                    account.schedule.clone(),
                    running.then(|| DetectorSnapshot {
                        account_id: account_id.clone(),
                        is_fallback: false,
                    }),
                    None,
                    self.account_courses
                        .get(account_id)
                        .cloned()
                        .unwrap_or_default(),
                    if running {
                        vec![account_id.clone()]
                    } else {
                        Vec::new()
                    },
                )
            }
            TargetId::Group { group_id } => {
                let group = config
                    .group(group_id)
                    .expect("schedulable target group exists");
                (
                    group.schedule.clone(),
                    self.actual_detectors.get(group_id).cloned(),
                    Some(GroupDefinitionSnapshot {
                        member_account_ids: group.member_account_ids.clone(),
                        course_ids: group.course_ids.clone(),
                        detector_selection: group.detector.clone(),
                    }),
                    group
                        .course_ids
                        .iter()
                        .map(|course_id| CourseSnapshot {
                            course_id: course_id.clone(),
                            name: course_id.clone(),
                        })
                        .collect(),
                    if running {
                        group.member_account_ids.clone()
                    } else {
                        Vec::new()
                    },
                )
            }
        };
        let disabled_reason = if all_suspended {
            Some("已一鍵停止全部；請先恢復照時間表".to_string())
        } else if platform_blocked {
            Some("平台背景執行限制尚未解除".to_string())
        } else if suppressed {
            Some("已在群組中監控；個人調整會在群組結束後生效".to_string())
        } else {
            state.clock_error.clone()
        };
        let account_results = self
            .account_results
            .iter()
            .filter_map(|((result_target, _), result)| {
                (result_target == &target).then_some(result.clone())
            })
            .collect();
        TargetSnapshot {
            target: target.clone(),
            name: target_name(config, &target),
            runtime_state,
            schedule,
            schedule_open: state.schedule_open,
            next_boundary_utc: state.next_boundary_utc,
            manual_override: state.manual_override,
            detector,
            group_definition,
            courses,
            in_use_account_ids,
            account_results,
            can_start: !all_suspended
                && !platform_blocked
                && !running
                && state.clock_error.is_none(),
            can_stop: !all_suspended
                && !platform_blocked
                && (running || suppressed)
                && state.clock_error.is_none(),
            can_edit_schedule: true,
            disabled_reason,
            error: None,
        }
    }

    fn effective_target_state(
        &self,
        config: &Config,
        target: &TargetId,
        now: i64,
    ) -> EffectiveTargetState {
        let persisted = config.runtime.manual_overrides.iter().find(|manual| {
            &manual.target == target
                && manual.schedule_revision == config.schedule_revision
                && parse_rfc3339_utc(&manual.expires_at_utc).is_some_and(|expiry| expiry > now)
        });
        let process = self
            .process_overrides
            .get(target)
            .filter(|manual| manual.schedule_revision == config.schedule_revision);
        match self.matching_clock_entry(config, target) {
            Ok(entry) => {
                let override_value = persisted
                    .map(|manual| manual.force_open)
                    .or_else(|| process.map(|manual| manual.force_open));
                EffectiveTargetState {
                    schedule_open: entry.is_open,
                    effective_open: override_value.unwrap_or(entry.is_open),
                    next_boundary_utc: entry.next_boundary_utc.clone(),
                    manual_override: persisted
                        .map(|manual| ManualOverrideSnapshot {
                            force_open: manual.force_open,
                            expires_at_utc: Some(manual.expires_at_utc.clone()),
                        })
                        .or_else(|| {
                            process.map(|manual| ManualOverrideSnapshot {
                                force_open: manual.force_open,
                                expires_at_utc: None,
                            })
                        }),
                    clock_error: None,
                }
            }
            Err(error) => EffectiveTargetState {
                schedule_open: false,
                effective_open: false,
                next_boundary_utc: None,
                manual_override: None,
                clock_error: Some(error),
            },
        }
    }

    fn matching_clock_entry<'a>(
        &'a self,
        config: &Config,
        target: &TargetId,
    ) -> Result<&'a ScheduleClockEntry, String> {
        let clock = self
            .clock
            .as_ref()
            .ok_or_else(|| "schedule_clock_unavailable".to_string())?;
        if clock.config_revision != config.config_revision
            || clock.schedule_revision != config.schedule_revision
        {
            return Err("schedule_clock_unavailable".to_string());
        }
        let entry = clock
            .entries
            .get(target)
            .ok_or_else(|| "schedule_clock_unavailable".to_string())?;
        if let Some(boundary) = entry
            .next_boundary_utc
            .as_deref()
            .and_then(parse_rfc3339_utc)
        {
            if now_epoch_seconds() > boundary + CLOCK_GRACE_SECONDS {
                return Err("schedule_clock_unavailable".to_string());
            }
        }
        // Monotonic age is a second fail-closed check for a backward wall-clock jump. Entries without
        // a boundary (Disabled/empty/invalid zone) intentionally remain valid until definitions change.
        if entry.next_boundary_utc.is_some()
            && clock.received_at.elapsed().as_secs() > 8 * 24 * 60 * 60
        {
            return Err("schedule_clock_unavailable".to_string());
        }
        Ok(entry)
    }

    fn select_detector(
        &self,
        config: &mut Config,
        group: &AccountGroup,
        state: &EffectiveTargetState,
    ) -> (Option<String>, bool, bool) {
        if group.member_account_ids.is_empty() {
            return (None, false, false);
        }
        let window_key = state
            .next_boundary_utc
            .as_ref()
            .and_then(|_| {
                self.clock
                    .as_ref()
                    .and_then(|clock| clock.entries.get(&TargetId::group(group.id.clone())))
                    .and_then(|entry| entry.window_key.clone())
            })
            .unwrap_or_else(|| "manual".to_string());
        let rotation = config
            .runtime
            .group_rotation
            .entry(group.id.clone())
            .or_default();
        rotation.next_index %= group.member_account_ids.len();
        let new_window = rotation.last_window_key.as_deref() != Some(window_key.as_str());
        let mut changed = false;
        let primary = match &group.detector {
            DetectorSelection::Preferred { account_id } => {
                if new_window {
                    rotation.last_window_key = Some(window_key);
                    changed = true;
                } else if let Some(actual) = self.actual_detectors.get(&group.id).filter(|actual| {
                    actual.is_fallback && !self.terminal_accounts.contains(&actual.account_id)
                }) {
                    return (Some(actual.account_id.clone()), true, changed);
                }
                account_id.clone()
            }
            DetectorSelection::Auto => {
                if new_window {
                    let selected = group.member_account_ids[rotation.next_index].clone();
                    rotation.next_index =
                        (rotation.next_index + 1) % group.member_account_ids.len();
                    rotation.last_window_key = Some(window_key);
                    changed = true;
                    selected
                } else {
                    self.actual_detectors
                        .get(&group.id)
                        .map(|detector| detector.account_id.clone())
                        .unwrap_or_else(|| {
                            let previous = (rotation.next_index + group.member_account_ids.len()
                                - 1)
                                % group.member_account_ids.len();
                            group.member_account_ids[previous].clone()
                        })
                }
            }
        };
        if !self.terminal_accounts.contains(&primary) {
            return (Some(primary), false, changed);
        }
        for offset in 0..group.member_account_ids.len() {
            let index = (rotation.next_index + offset) % group.member_account_ids.len();
            let candidate = &group.member_account_ids[index];
            if !self.terminal_accounts.contains(candidate) {
                return (Some(candidate.clone()), true, changed);
            }
        }
        (None, true, changed)
    }

    fn merge_prompts_for(
        &self,
        _config: &Config,
        active_groups: &[(AccountGroup, EffectiveTargetState)],
        routes: &[EffectiveRoute],
    ) -> Vec<MergePrompt> {
        let components =
            connected_group_components(active_groups.iter().map(|(group, _)| group).collect());
        components
            .into_iter()
            .filter(|component| component.len() > 1)
            .map(|component| {
                let mut group_ids: Vec<String> =
                    component.iter().map(|group| group.id.clone()).collect();
                group_ids.sort();
                let component_id = group_ids.join("+");
                let required_courses: HashSet<String> = component
                    .iter()
                    .flat_map(|group| {
                        if group.course_ids.is_empty() {
                            self.actual_detectors
                                .get(&group.id)
                                .and_then(|detector| self.account_courses.get(&detector.account_id))
                                .into_iter()
                                .flatten()
                                .map(|course| course.course_id.clone())
                                .collect::<Vec<_>>()
                        } else {
                            group.course_ids.clone()
                        }
                    })
                    .collect();
                let candidates: HashSet<&String> = component
                    .iter()
                    .flat_map(|group| group.member_account_ids.iter())
                    .collect();
                let detector = candidates.into_iter().find(|candidate| {
                    self.account_courses
                        .get(*candidate)
                        .map(|courses| {
                            let available: HashSet<&str> = courses
                                .iter()
                                .map(|course| course.course_id.as_str())
                                .collect();
                            required_courses
                                .iter()
                                .all(|course| available.contains(course.as_str()))
                        })
                        .unwrap_or(required_courses.is_empty())
                });
                let detector_count = routes
                    .iter()
                    .filter(|route| {
                        route.source_targets.iter().any(|target| match target {
                            TargetId::Group { group_id } => group_ids.contains(group_id),
                            TargetId::Account { .. } => false,
                        })
                    })
                    .map(|route| route.detector_account_id.as_str())
                    .collect::<HashSet<_>>()
                    .len() as u32;
                MergePrompt {
                    component_id: component_id.clone(),
                    group_ids,
                    coverage: if detector.is_some() {
                        MergeCoverage::SingleDetector
                    } else {
                        MergeCoverage::MultipleDetectorsRequired
                    },
                    detector_account_id: detector.cloned(),
                    detector_count: detector_count.max(1),
                    warning: detector.is_none().then(|| {
                        format!(
                            "無單一帳號可完整涵蓋課程，目前保留 {} 支監控帳號",
                            detector_count.max(1)
                        )
                    }),
                    acknowledged: self.acknowledged_merges.contains(&component_id),
                }
            })
            .collect()
    }

    fn bump_plan_revision(&mut self) {
        self.plan_revision = self.plan_revision.saturating_add(1);
    }
}

fn validate_clock_entry(
    config: &Config,
    entry: &ScheduleClockEntry,
    evaluated_at_epoch: i64,
) -> Result<(), String> {
    ensure_target_exists(config, &entry.target)?;
    let current = entry
        .current_window_start_utc
        .as_deref()
        .map(parse_rfc3339_utc)
        .transpose_option()
        .ok_or_else(|| "schedule_clock_invalid_timestamp".to_string())?;
    let next = entry
        .next_boundary_utc
        .as_deref()
        .map(parse_rfc3339_utc)
        .transpose_option()
        .ok_or_else(|| "schedule_clock_invalid_timestamp".to_string())?;
    if entry
        .clock_error
        .as_deref()
        .is_some_and(|error| error != "invalid_time_zone")
    {
        return Err("schedule_clock_invalid_error".to_string());
    }
    if entry.clock_error.is_some()
        && (entry.is_open
            || entry.window_key.is_some()
            || current.is_some()
            || next.is_some()
            || entry.next_is_open.is_some())
    {
        return Err("schedule_clock_invalid_entry".to_string());
    }
    if entry.is_open {
        if entry.window_key.as_deref().is_none_or(str::is_empty)
            || current.is_none()
            || next.is_none()
            || entry.next_is_open != Some(false)
            || current.is_some_and(|start| start > evaluated_at_epoch)
            || next.is_some_and(|end| end <= evaluated_at_epoch)
        {
            return Err("schedule_clock_invalid_entry".to_string());
        }
    } else if next.is_some() != entry.next_is_open.is_some()
        || entry.next_is_open.is_some_and(|next_open| !next_open)
        || next.is_some_and(|boundary| boundary <= evaluated_at_epoch)
        || entry.window_key.is_some()
        || current.is_some()
    {
        return Err("schedule_clock_invalid_entry".to_string());
    }

    let binding = match &entry.target {
        TargetId::Account { account_id } => {
            &config
                .account(account_id)
                .ok_or_else(|| "unknown_target".to_string())?
                .schedule
        }
        TargetId::Group { group_id } => {
            &config
                .group(group_id)
                .ok_or_else(|| "unknown_target".to_string())?
                .schedule
        }
    };
    let empty = match binding {
        ScheduleBinding::Disabled => true,
        ScheduleBinding::InheritGlobal => config.monitoring.global_schedule.is_empty(),
        ScheduleBinding::Custom { weekly } => weekly.is_empty(),
    };
    if empty
        && (entry.is_open
            || entry.window_key.is_some()
            || current.is_some()
            || next.is_some()
            || entry.next_is_open.is_some()
            || entry.clock_error.is_some())
    {
        return Err("schedule_clock_invalid_entry".to_string());
    }
    Ok(())
}

trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}

impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            None => Some(None),
            Some(Some(value)) => Some(Some(value)),
            Some(None) => None,
        }
    }
}

fn schedulable_targets(config: &Config) -> Vec<TargetId> {
    config
        .accounts
        .iter()
        .filter(|account| !account.is_teacher)
        .map(|account| TargetId::account(account.id.clone()))
        .chain(
            config
                .groups
                .iter()
                .map(|group| TargetId::group(group.id.clone())),
        )
        .collect()
}

fn ensure_target_exists(config: &Config, target: &TargetId) -> Result<(), String> {
    match target {
        TargetId::Account { account_id } => config
            .account(account_id)
            .filter(|account| !account.is_teacher)
            .map(|_| ())
            .ok_or_else(|| "unknown_target".to_string()),
        TargetId::Group { group_id } => config
            .group(group_id)
            .map(|_| ())
            .ok_or_else(|| "unknown_target".to_string()),
    }
}

fn target_name(config: &Config, target: &TargetId) -> String {
    match target {
        TargetId::Account { account_id } => config
            .account(account_id)
            .map(|account| account.label.clone())
            .unwrap_or_else(|| account_id.clone()),
        TargetId::Group { group_id } => config
            .group(group_id)
            .map(|group| group.name.clone())
            .unwrap_or_else(|| group_id.clone()),
    }
}

fn connected_group_components(groups: Vec<&AccountGroup>) -> Vec<Vec<&AccountGroup>> {
    let mut remaining: VecDeque<_> = groups.into();
    let mut components = Vec::new();
    while let Some(seed) = remaining.pop_front() {
        let mut component = vec![seed];
        let mut members: HashSet<&str> =
            seed.member_account_ids.iter().map(String::as_str).collect();
        loop {
            let mut changed = false;
            let mut index = 0;
            while index < remaining.len() {
                if remaining[index]
                    .member_account_ids
                    .iter()
                    .any(|member| members.contains(member.as_str()))
                {
                    let group = remaining.remove(index).expect("index is in bounds");
                    members.extend(group.member_account_ids.iter().map(String::as_str));
                    component.push(group);
                    changed = true;
                } else {
                    index += 1;
                }
            }
            if !changed {
                break;
            }
        }
        components.push(component);
    }
    components
}

fn coalesce_acknowledged(
    routes: &mut Vec<EffectiveRoute>,
    prompts: &[MergePrompt],
    acknowledged: &HashSet<String>,
    config: &Config,
) {
    for prompt in prompts.iter().filter(|prompt| {
        acknowledged.contains(&prompt.component_id)
            && prompt.coverage == MergeCoverage::SingleDetector
            && prompt.detector_account_id.is_some()
    }) {
        let mut sources = Vec::new();
        let mut participants = Vec::new();
        let mut courses = Vec::new();
        routes.retain(|route| {
            let belongs = route.source_targets.iter().any(|target| match target {
                TargetId::Group { group_id } => prompt.group_ids.contains(group_id),
                TargetId::Account { .. } => false,
            });
            if belongs {
                sources.extend(route.source_targets.iter().cloned());
                participants.extend(route.participant_account_ids.iter().cloned());
                courses.extend(route.course_ids.iter().cloned());
            }
            !belongs
        });
        for group_id in &prompt.group_ids {
            if let Some(group) = config.group(group_id) {
                participants.extend(group.member_account_ids.iter().cloned());
                courses.extend(group.course_ids.iter().cloned());
            }
        }
        sources.sort_by_key(TargetId::stable_key);
        sources.dedup();
        participants.sort();
        participants.dedup();
        courses.sort();
        courses.dedup();
        routes.push(EffectiveRoute {
            source_targets: sources,
            detector_account_id: prompt.detector_account_id.clone().expect("filtered Some"),
            participant_account_ids: participants,
            course_ids: courses,
        });
    }
}

pub fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn parse_rfc3339_utc(value: &str) -> Option<i64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date = date.split('-');
    let year: i32 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    if date.next().is_some()
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
    {
        return None;
    }
    let time = time.split_once('.').map_or(time, |(whole, fraction)| {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            ""
        } else {
            whole
        }
    });
    if time.is_empty() {
        return None;
    }
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;
    if time.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

pub fn format_rfc3339_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's civil calendar conversion, with Unix epoch 1970-01-01.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146_097 + doe - 719_468)
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountMeta;

    fn student(id: &str, schedule: ScheduleBinding) -> AccountMeta {
        AccountMeta {
            id: id.into(),
            label: id.into(),
            school_ref: "https://example.test".into(),
            username: id.into(),
            device_id: format!("device-{id}"),
            is_teacher: false,
            course_id: None,
            schedule,
        }
    }

    fn disabled_entry(id: &str) -> ScheduleClockEntry {
        ScheduleClockEntry {
            target: TargetId::account(id),
            is_open: false,
            window_key: None,
            current_window_start_utc: None,
            next_boundary_utc: None,
            next_is_open: None,
            clock_error: None,
        }
    }

    #[test]
    fn rfc3339_round_trip_handles_leap_day_and_negative_epoch() {
        for value in ["1969-12-31T23:59:59Z", "2028-02-29T12:34:56.1234567Z"] {
            let epoch = parse_rfc3339_utc(value).unwrap();
            assert_eq!(parse_rfc3339_utc(&format_rfc3339_utc(epoch)), Some(epoch));
        }
        assert!(parse_rfc3339_utc("2026-02-29T00:00:00Z").is_none());
        assert!(parse_rfc3339_utc("2026-01-01T00:00:00+00:00").is_none());
    }

    #[test]
    fn clock_requires_exact_targets_matching_revisions() {
        let mut config = Config::default();
        config
            .accounts
            .push(student("a", ScheduleBinding::Disabled));
        config.config_revision = 2;
        config.schedule_revision = 3;
        let mut supervisor = TargetSupervisor::new(None);
        let now = format_rfc3339_utc(now_epoch_seconds());
        assert!(supervisor
            .apply_clock(&config, 1, 2, 3, &now, vec![])
            .is_err());
        supervisor
            .apply_clock(&config, 1, 2, 3, &now, vec![disabled_entry("a")])
            .unwrap();
        assert!(supervisor
            .apply_clock(&config, 1, 2, 3, &now, vec![disabled_entry("a")])
            .is_err());
        assert!(supervisor
            .apply_clock(&config, 2, 2, 4, &now, vec![disabled_entry("a")])
            .is_err());
    }

    #[test]
    fn disabled_target_uses_process_override_without_persistence() {
        let mut config = Config::default();
        config
            .accounts
            .push(student("a", ScheduleBinding::Disabled));
        let mut supervisor = TargetSupervisor::new(None);
        let now = format_rfc3339_utc(now_epoch_seconds());
        supervisor
            .apply_clock(&config, 1, 0, 0, &now, vec![disabled_entry("a")])
            .unwrap();
        assert!(!supervisor
            .set_target_override(&mut config, &TargetId::account("a"), true)
            .unwrap());
        supervisor.reconcile(&mut config);
        assert_eq!(supervisor.plan.routes.len(), 1);
        assert!(config.runtime.manual_overrides.is_empty());
    }

    #[test]
    fn group_suppresses_personal_and_rotation_advances_once_per_window() {
        let mut config = Config::default();
        config
            .accounts
            .push(student("a", ScheduleBinding::Disabled));
        config
            .accounts
            .push(student("b", ScheduleBinding::Disabled));
        config.groups.push(AccountGroup {
            id: "g".into(),
            name: "group".into(),
            tenant: "https://example.test".into(),
            member_account_ids: vec!["a".into(), "b".into()],
            course_ids: Vec::new(),
            detector: DetectorSelection::Auto,
            schedule: ScheduleBinding::Disabled,
        });
        let mut supervisor = TargetSupervisor::new(None);
        let now = format_rfc3339_utc(now_epoch_seconds());
        supervisor
            .apply_clock(
                &config,
                1,
                0,
                0,
                &now,
                vec![
                    disabled_entry("a"),
                    disabled_entry("b"),
                    ScheduleClockEntry {
                        target: TargetId::group("g"),
                        is_open: false,
                        window_key: None,
                        current_window_start_utc: None,
                        next_boundary_utc: None,
                        next_is_open: None,
                        clock_error: None,
                    },
                ],
            )
            .unwrap();
        supervisor
            .set_target_override(&mut config, &TargetId::account("a"), true)
            .unwrap();
        supervisor
            .set_target_override(&mut config, &TargetId::group("g"), true)
            .unwrap();
        assert!(supervisor.reconcile(&mut config));
        assert!(supervisor.plan.suppressed_accounts.contains("a"));
        assert_eq!(supervisor.plan.routes.len(), 1);
        assert_eq!(config.runtime.group_rotation["g"].next_index, 1);
        assert!(!supervisor.reconcile(&mut config));
        assert_eq!(config.runtime.group_rotation["g"].next_index, 1);
    }
    #[test]
    fn preferred_detector_does_not_switch_back_within_same_window() {
        let mut config = Config::default();
        config
            .accounts
            .push(student("a", ScheduleBinding::Disabled));
        config
            .accounts
            .push(student("b", ScheduleBinding::Disabled));
        config.groups.push(AccountGroup {
            id: "g".into(),
            name: "group".into(),
            tenant: "https://example.test".into(),
            member_account_ids: vec!["a".into(), "b".into()],
            course_ids: Vec::new(),
            detector: DetectorSelection::Preferred {
                account_id: "a".into(),
            },
            schedule: ScheduleBinding::Disabled,
        });
        let mut supervisor = TargetSupervisor::new(None);
        let now = format_rfc3339_utc(now_epoch_seconds());
        supervisor
            .apply_clock(
                &config,
                1,
                0,
                0,
                &now,
                vec![
                    disabled_entry("a"),
                    disabled_entry("b"),
                    ScheduleClockEntry {
                        target: TargetId::group("g"),
                        is_open: false,
                        window_key: None,
                        current_window_start_utc: None,
                        next_boundary_utc: None,
                        next_is_open: None,
                        clock_error: None,
                    },
                ],
            )
            .unwrap();
        supervisor
            .set_target_override(&mut config, &TargetId::group("g"), true)
            .unwrap();
        supervisor.set_login_state(
            "a",
            LoginState::Error,
            Some(WireError {
                code: "login_failed".into(),
                message: "failed".into(),
            }),
        );
        supervisor.set_login_state("b", LoginState::Online, None);
        supervisor.reconcile(&mut config);
        assert_eq!(
            supervisor.actual_detectors["g"],
            DetectorSnapshot {
                account_id: "b".into(),
                is_fallback: true,
            }
        );
        supervisor.set_login_state("a", LoginState::Online, None);
        supervisor.reconcile(&mut config);
        assert_eq!(
            supervisor.actual_detectors["g"].account_id, "b",
            "preferred recovery must wait for the next window"
        );
    }
}

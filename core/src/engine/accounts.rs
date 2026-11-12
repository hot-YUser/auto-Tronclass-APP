//! Adding and removing accounts. Each change spans the config file and the vault, so it is
//! journaled and a crash in between is finished or undone on the next start.

use super::commands::{Handled, Reply};
use super::definitions::check_revision;
use super::monitoring::emit_monitoring_snapshot;
use super::state::CoreState;
use crate::store::config::{new_id, AccountMeta, Config, DetectorSelection, ScheduleBinding};
use crate::store::journal::{AccountJournal, AccountMutation};
use crate::store::secrets::{AccountSecret, Secret, VaultFile};
use crate::wire::{emit, Event, EventCb, LogLevel, Problem};
use serde_json::json;

pub(super) fn recover_account_transaction(
    dir: &std::path::Path,
    config: &Config,
    vault: &mut VaultFile,
) -> Result<Option<String>, String> {
    let Some(journal) = AccountJournal::load(dir)? else {
        return Ok(None);
    };
    let account_exists = config.account(&journal.account_id).is_some();
    // In either ordered transaction, an absent config record means the durable end state must not
    // retain a secret: rollback an unfinished Add or roll forward an already-configured Delete.
    if !account_exists {
        vault.delete(&journal.account_id)?;
    }
    AccountJournal::complete(dir)?;
    Ok(Some(format!(
        "已恢復未完成的帳號{}交易",
        match journal.mutation {
            AccountMutation::Add => "新增",
            AccountMutation::Delete => "刪除",
        }
    )))
}

/// Lazy journal recovery before an account mutation. A crash between `begin` and `complete` leaves
/// a stale journal; resolving it FIRST means a leftover record can never wedge the next mutation.
/// Success warns with the recovery message; failure returns the fixed error — the journal loader's
/// own error can echo file content, so it must never cross the seam. The caller fails the mutation
/// on `Err`; the vault must already be unlocked.
pub(super) fn recover_stale_journal(st: &mut CoreState, cb: EventCb) -> Result<(), String> {
    let Some(vault) = st.vault.as_mut() else {
        return Err("vault is locked".to_string());
    };
    match recover_account_transaction(&st.data_dir, &st.config, vault) {
        Ok(Some(message)) => {
            emit(cb, Event::log(LogLevel::Warn, message));
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(_) => Err("account transaction journal unreadable; recovery skipped".to_string()),
    }
}

pub(super) struct NewAccount {
    pub label: String,
    /// A registry key or alias, or a base URL; kept as the user gave it.
    pub school: String,
    pub username: String,
    pub password: Secret,
    pub is_teacher: bool,
    pub course_id: Option<String>,
}

/// Store the password, then record the account. If the record cannot be saved the password is
/// removed again; if even that fails, the journal is left for the next start to finish the job.
pub(super) fn add(st: &mut CoreState, cb: EventCb, new: NewAccount) -> Handled {
    st.vault_mut()?;
    recover_stale_journal(st, cb)?;
    let account = AccountMeta {
        id: new_id(),
        label: new.label,
        school_ref: new.school,
        username: new.username,
        // A random device code that stays with the account.
        device_id: new_id(),
        is_teacher: new.is_teacher,
        course_id: new.course_id,
        schedule: ScheduleBinding::Disabled,
    };
    let account_id = account.id.clone();
    let secret = AccountSecret {
        password: new.password.expose().to_string(),
        cookies: String::new(),
    };

    AccountJournal::begin(&st.data_dir, AccountMutation::Add, &account_id)?;
    if let Err(error) = st.vault_mut()?.set(&account_id, secret) {
        let _ = AccountJournal::complete(&st.data_dir);
        return Err(error.into());
    }
    let previous = st.config.clone();
    st.config.accounts.push(account);
    let saved = st
        .config
        .bump_definition_revision(true)
        .and_then(|()| st.config.save(&st.config_path()));
    if let Err(error) = saved {
        st.config = previous;
        return Err(match st.vault_mut()?.delete(&account_id) {
            Ok(()) => {
                let _ = AccountJournal::complete(&st.data_dir);
                error
            }
            Err(undo) => {
                format!("{error}; vault rollback failed and will retry on restart: {undo}")
            }
        }
        .into());
    }
    finish(st, cb);
    Ok(Reply::Data(json!({ "account_id": account_id })))
}

/// Remove the account's record, then its password. An account that a group still names, or that
/// the running plan still uses, is not removed.
pub(super) fn delete(
    st: &mut CoreState,
    cb: EventCb,
    account_id: &str,
    expected_revision: u64,
    remove_from_groups: bool,
) -> Handled {
    check_revision(st, expected_revision)?;
    if st.config.account(account_id).is_none() {
        return Err("no such account".into());
    }
    st.vault_mut()?;
    let member_of = |members: &[String]| members.iter().any(|member| member == account_id);
    if !remove_from_groups
        && st
            .config
            .groups
            .iter()
            .any(|group| member_of(&group.member_account_ids))
    {
        return Err("account_is_referenced_by_groups".into());
    }
    if st.supervisor.plan().routes.iter().any(|route| {
        route.detector_account_id == account_id || member_of(&route.participant_account_ids)
    }) {
        return Err("account_is_in_use".into());
    }
    recover_stale_journal(st, cb)?;
    // A login waiting on a captcha for this account wakes to a closed channel and ends.
    st.pending_captcha.remove(account_id);
    st.login_in_flight.remove(account_id);

    AccountJournal::begin(&st.data_dir, AccountMutation::Delete, account_id)?;
    let previous = st.config.clone();
    forget_account(&mut st.config, account_id);
    let saved = st
        .config
        .bump_definition_revision(true)
        .and_then(|()| st.config.save(&st.config_path()));
    if let Err(error) = saved {
        st.config = previous;
        let _ = AccountJournal::complete(&st.data_dir);
        return Err(error.into());
    }
    if let Err(vault_error) = st.vault_mut()?.delete(account_id) {
        // Put the record back. If that cannot be saved either, the journal stays so the next
        // start removes the password.
        let without = std::mem::replace(&mut st.config, previous);
        return Err(match st.config.save(&st.config_path()) {
            Ok(()) => {
                let _ = AccountJournal::complete(&st.data_dir);
                format!("delete vault entry failed: {vault_error}")
            }
            Err(config_error) => {
                st.config = without;
                format!(
                    "delete vault entry failed: {vault_error}; config rollback failed and recovery journal remains: {config_error}"
                )
            }
        }
        .into());
    }
    finish(st, cb);
    Ok(Reply::Done)
}

/// Drop the account from the definitions: its record, its group memberships, any group it leaves
/// empty, and any group's preference for it as detector.
fn forget_account(config: &mut Config, account_id: &str) {
    config.accounts.retain(|account| account.id != account_id);
    for group in &mut config.groups {
        group
            .member_account_ids
            .retain(|member| member != account_id);
        if matches!(&group.detector, DetectorSelection::Preferred { account_id: preferred } if preferred == account_id)
        {
            group.detector = DetectorSelection::Auto;
        }
    }
    config
        .groups
        .retain(|group| !group.member_account_ids.is_empty());
}

/// The change is on disk on both sides: close the journal and publish the new state.
fn finish(st: &mut CoreState, cb: EventCb) {
    if let Err(error) = AccountJournal::complete(&st.data_dir) {
        emit(
            cb,
            Problem::warning("account_transaction_cleanup_failed", error),
        );
    }
    st.supervisor.invalidate_clock();
    st.supervisor.reconcile(&mut st.config);
    emit_monitoring_snapshot(cb, st);
}

//! Policy enforcement, state administration, audit, and metrics.
//!
//! Part of the [`CapsuleManager`] implementation; see the parent module.

// This file is a continuation of the parent module's `impl CapsuleManager`,
// so it deliberately shares the parent's imports rather than duplicating a
// large, constantly churning list.
#[allow(clippy::wildcard_imports)]
use super::*;

impl CapsuleManager {
    /// Read the effective policy; permissive defaults when none is installed.
    pub fn policy(&self) -> Result<Policy> {
        self.store.read_policy()
    }

    /// Replace the stored policy atomically.
    ///
    /// Repository roots are canonicalized and must be existing directories.
    pub fn set_policy(&self, mut policy: Policy) -> Result<Policy> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        for root in &mut policy.allowed_repository_roots {
            *root = canonical_existing(root)?;
            if !root.is_dir() {
                return Err(Error::PolicyViolation(format!(
                    "allowed repository root is not a directory: {}",
                    root.display()
                )));
            }
        }
        policy.allowed_repository_roots.sort();
        policy.allowed_repository_roots.dedup();
        policy.validate()?;
        self.store.write_policy(&policy)?;
        Ok(policy)
    }

    /// Evaluate current state against the effective policy without mutating it.
    pub fn policy_report(&self) -> Result<PolicyReport> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        let policy = self.store.read_policy()?;
        let capsules = self.store.list_capsules()?;
        let violations = self.policy_violations(&policy, &capsules)?;
        Ok(PolicyReport {
            compliant: violations.is_empty(),
            violations,
        })
    }

    /// Summarize stored records, including ones whose schema is unsupported.
    pub fn inspect_state(&self) -> Result<StateInspection> {
        self.store.inspect()
    }

    /// Copy durable manifests, results, patches, and policy to a new directory.
    ///
    /// Live workspaces and Git object databases are deliberately excluded.
    /// `backup.json` is written last as the completion marker.
    pub fn backup_state(&self, destination: impl AsRef<Path>) -> Result<BackupReport> {
        self.store.backup(destination.as_ref())
    }

    /// Explicitly migrate schema-v3 durable manifests/results to schema v4.
    ///
    /// Dry-run validates every candidate without mutation. Apply requires a new
    /// external backup destination and writes the complete backup before state.
    pub fn migrate_state_v3(
        &self,
        backup: Option<&Path>,
        apply: bool,
    ) -> Result<crate::model::MigrationReport> {
        self.store.migrate_v3(backup, apply)
    }

    /// Retained lifecycle events for one capsule, oldest first.
    pub fn audit_events(&self, id: &str) -> Result<Vec<AuditEvent>> {
        Ok(self.show(id)?.audit_events)
    }

    /// Retained lifecycle events across all capsules, merged in time order.
    pub fn audit_log(&self) -> Result<Vec<AuditEvent>> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        let mut sequenced = Vec::new();
        for capsule in self.store.list_capsules()? {
            for (sequence, event) in capsule.audit_events.into_iter().enumerate() {
                sequenced.push((capsule.id.clone(), sequence, event));
            }
        }
        sequenced.sort_by(|left, right| {
            left.2
                .occurred_at_unix
                .cmp(&right.2.occurred_at_unix)
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| left.1.cmp(&right.1))
        });
        Ok(sequenced.into_iter().map(|(_, _, event)| event).collect())
    }

    /// Compute an instantaneous snapshot of aggregate counters.
    pub fn metrics(&self) -> Result<MetricsSnapshot> {
        let _lock = self.store.lock_global()?;
        let _project_locks = self.store.lock_all_projects()?;
        let capsules = self.store.list_capsules()?;
        let mut states = BTreeMap::new();
        let mut live_capsules = 0_u64;
        let mut sealed_results = 0_u64;
        let mut result_patch_bytes = 0_u64;
        let mut audit_events = 0_u64;
        let mut audit_events_dropped = 0_u64;
        for capsule in &capsules {
            *states
                .entry(state_name(capsule.state).to_owned())
                .or_insert(0) += 1;
            if capsule.state != CapsuleState::Dropped {
                live_capsules += 1;
            }
            if let Some(result) = &capsule.result {
                sealed_results += 1;
                result_patch_bytes = result_patch_bytes.saturating_add(result.patch_bytes);
            }
            audit_events = audit_events.saturating_add(capsule.audit_events.len() as u64);
            audit_events_dropped =
                audit_events_dropped.saturating_add(capsule.audit_events_dropped);
        }
        Ok(MetricsSnapshot {
            observed_at_unix: now()?,
            capsules: capsules.len() as u64,
            live_capsules,
            sealed_results,
            result_patch_bytes,
            state_bytes: self.store.state_bytes()?,
            workspace_bytes: self.store.workspace_bytes()?,
            audit_events,
            audit_events_dropped,
            states,
        })
    }

    pub(super) fn policy_violations(
        &self,
        policy: &Policy,
        capsules: &[Capsule],
    ) -> Result<Vec<String>> {
        policy.validate()?;
        let mut violations = Vec::new();
        let capsule_count = capsules.len() as u64;
        let live_count = capsules
            .iter()
            .filter(|capsule| capsule.state != CapsuleState::Dropped)
            .count() as u64;
        check_limit(
            &mut violations,
            "capsule records",
            capsule_count,
            policy.max_capsules,
        );
        check_limit(
            &mut violations,
            "live capsules",
            live_count,
            policy.max_live_capsules,
        );
        if policy.max_state_bytes.is_some() {
            check_limit(
                &mut violations,
                "state bytes",
                self.store.state_bytes()?,
                policy.max_state_bytes,
            );
        }
        if policy.max_workspace_bytes.is_some() {
            check_limit(
                &mut violations,
                "workspace bytes",
                self.store.workspace_bytes()?,
                policy.max_workspace_bytes,
            );
        }
        let observed_at = now()?;
        for capsule in capsules {
            violations.extend(self.capsule_policy_violations(policy, capsule, observed_at));
        }
        Ok(violations)
    }

    pub(super) fn capsule_policy_violations(
        &self,
        policy: &Policy,
        capsule: &Capsule,
        observed_at: u64,
    ) -> Vec<String> {
        let mut violations = Vec::new();
        if !repository_allowed(policy, &capsule.source_worktree) {
            violations.push(format!(
                "capsule {} repository is outside allowed roots: {}",
                capsule.id,
                capsule.source_worktree.display()
            ));
        }
        if capsule.state != CapsuleState::Dropped {
            if let Some(limit) = policy.max_capsule_age_seconds {
                let age = observed_at.saturating_sub(capsule.created_at_unix);
                if age > limit {
                    violations.push(format!(
                        "capsule {} age {age} seconds exceeds limit {limit}",
                        capsule.id
                    ));
                }
            }
        }
        if let Some(reference) = &capsule.result {
            self.sealed_capsule_policy_violations(policy, capsule, reference, &mut violations);
        } else {
            self.active_capsule_policy_violations(policy, capsule, &mut violations);
        }
        violations
    }

    pub(super) fn sealed_capsule_policy_violations(
        &self,
        policy: &Policy,
        capsule: &Capsule,
        reference: &ResultRef,
        violations: &mut Vec<String>,
    ) {
        check_capsule_limit(
            violations,
            &capsule.id,
            "patch bytes",
            reference.patch_bytes,
            Some(policy.max_patch_bytes),
        );
        check_capsule_limit(
            violations,
            &capsule.id,
            "changed paths",
            reference.changed_paths as u64,
            policy.max_changed_paths,
        );
        let result = match self.sealed_artifacts(capsule) {
            Ok((true, result, _)) => result,
            Ok((false, _, _)) => {
                violations.push(format!(
                    "capsule {} sealed result artifacts do not match their manifest",
                    capsule.id
                ));
                return;
            }
            Err(error) => {
                violations.push(format!(
                    "capsule {} result cannot be inspected: {error}",
                    capsule.id
                ));
                return;
            }
        };
        if policy.max_ignored_paths.is_none() && policy.max_ignored_bytes.is_none() {
            return;
        }
        let (ignored_paths, ignored_bytes) = if capsule.workspace_path.exists() {
            if let Err(error) = self.validate_owned_worktree(capsule) {
                violations.push(format!(
                    "capsule {} workspace cannot be inspected: {error}",
                    capsule.id
                ));
                (result.ignored_paths.len() as u64, result.ignored_bytes)
            } else {
                match self.git.ignored_paths(&capsule.workspace_path) {
                    Ok(paths) => {
                        let bytes = if policy.max_ignored_bytes.is_some() {
                            match ignored_usage(&capsule.workspace_path, &paths) {
                                Ok(bytes) => bytes,
                                Err(error) => {
                                    violations.push(format!(
                                        "capsule {} ignored bytes cannot be inspected: {error}",
                                        capsule.id
                                    ));
                                    result.ignored_bytes
                                }
                            }
                        } else {
                            0
                        };
                        (paths.len() as u64, bytes)
                    }
                    Err(error) => {
                        violations.push(format!(
                            "capsule {} ignored paths cannot be inspected: {error}",
                            capsule.id
                        ));
                        (result.ignored_paths.len() as u64, result.ignored_bytes)
                    }
                }
            }
        } else {
            (result.ignored_paths.len() as u64, result.ignored_bytes)
        };
        check_capsule_limit(
            violations,
            &capsule.id,
            "ignored paths",
            ignored_paths,
            policy.max_ignored_paths,
        );
        check_capsule_limit(
            violations,
            &capsule.id,
            "ignored bytes",
            ignored_bytes,
            policy.max_ignored_bytes,
        );
    }

    pub(super) fn active_capsule_policy_violations(
        &self,
        policy: &Policy,
        capsule: &Capsule,
        violations: &mut Vec<String>,
    ) {
        if !matches!(
            capsule.state,
            CapsuleState::Active | CapsuleState::Checkpointing
        ) {
            return;
        }
        if let Err(error) = self.validate_owned_worktree(capsule) {
            violations.push(format!(
                "capsule {} active result usage cannot be inspected: {error}",
                capsule.id
            ));
            return;
        }
        match self.snapshot(capsule) {
            Ok(snapshot) => {
                check_capsule_limit(
                    violations,
                    &capsule.id,
                    "patch bytes",
                    snapshot.patch.len() as u64,
                    Some(policy.max_patch_bytes),
                );
                check_capsule_limit(
                    violations,
                    &capsule.id,
                    "changed paths",
                    snapshot.changed_paths.len() as u64,
                    policy.max_changed_paths,
                );
            }
            Err(error) => violations.push(format!(
                "capsule {} active result usage cannot be inspected: {error}",
                capsule.id
            )),
        }
        if policy.max_ignored_paths.is_none() && policy.max_ignored_bytes.is_none() {
            return;
        }
        match self.git.ignored_paths(&capsule.workspace_path) {
            Ok(ignored) => {
                check_capsule_limit(
                    violations,
                    &capsule.id,
                    "ignored paths",
                    ignored.len() as u64,
                    policy.max_ignored_paths,
                );
                if policy.max_ignored_bytes.is_some() {
                    match ignored_usage(&capsule.workspace_path, &ignored) {
                        Ok(bytes) => check_capsule_limit(
                            violations,
                            &capsule.id,
                            "ignored bytes",
                            bytes,
                            policy.max_ignored_bytes,
                        ),
                        Err(error) => violations.push(format!(
                            "capsule {} ignored bytes cannot be inspected: {error}",
                            capsule.id
                        )),
                    }
                }
            }
            Err(error) => violations.push(format!(
                "capsule {} ignored paths cannot be inspected: {error}",
                capsule.id
            )),
        }
    }

    pub(super) fn enforce_create_policy(
        &self,
        policy: &Policy,
        capsules: &[Capsule],
        repository: &Repository,
        unmaterialized_reservations: u64,
    ) -> Result<()> {
        policy.validate()?;
        if !repository_allowed(policy, &repository.worktree) {
            return Err(Error::PolicyViolation(format!(
                "repository is outside allowed roots: {}",
                repository.worktree.display()
            )));
        }
        let capsule_records = (capsules.len() as u64)
            .checked_add(unmaterialized_reservations)
            .ok_or_else(|| Error::PolicyViolation("capsule record count overflowed".to_owned()))?;
        enforce_next_limit("capsule records", capsule_records, policy.max_capsules)?;
        let live_capsules = (capsules
            .iter()
            .filter(|capsule| capsule.state != CapsuleState::Dropped)
            .count() as u64)
            .checked_add(unmaterialized_reservations)
            .ok_or_else(|| Error::PolicyViolation("live capsule count overflowed".to_owned()))?;
        enforce_next_limit("live capsules", live_capsules, policy.max_live_capsules)?;
        if policy.max_state_bytes.is_some() {
            enforce_limit(
                "state bytes",
                self.store.state_bytes()?,
                policy.max_state_bytes,
            )?;
        }
        if policy.max_workspace_bytes.is_some() {
            enforce_limit(
                "workspace bytes",
                self.store.workspace_bytes()?,
                policy.max_workspace_bytes,
            )?;
        }
        Ok(())
    }

    pub(super) fn enforce_capsule_policy(&self, capsule: &Capsule) -> Result<Policy> {
        let policy = self.store.read_policy()?;
        if !repository_allowed(&policy, &capsule.source_worktree) {
            return Err(Error::PolicyViolation(format!(
                "capsule repository is outside allowed roots: {}",
                capsule.source_worktree.display()
            )));
        }
        if let Some(limit) = policy.max_capsule_age_seconds {
            let age = now()?.saturating_sub(capsule.created_at_unix);
            if age > limit {
                return Err(Error::PolicyViolation(format!(
                    "capsule age {age} seconds exceeds limit {limit}"
                )));
            }
        }
        if policy.max_state_bytes.is_some() {
            enforce_limit(
                "state bytes",
                self.store.state_bytes()?,
                policy.max_state_bytes,
            )?;
        }
        if policy.max_workspace_bytes.is_some() {
            enforce_limit(
                "workspace bytes",
                self.store.workspace_bytes()?,
                policy.max_workspace_bytes,
            )?;
        }
        Ok(policy)
    }

    /// Ignored-content usage is measured only when a policy limit makes it
    /// relevant; the byte walk uses file metadata rather than reading content.
    pub(super) fn ignored_usage_for_policy(
        &self,
        policy: &Policy,
        workspace: &Path,
    ) -> Result<(usize, u64)> {
        if policy.max_ignored_paths.is_none() && policy.max_ignored_bytes.is_none() {
            return Ok((0, 0));
        }
        let paths = self.git.ignored_paths(workspace)?;
        let bytes = if policy.max_ignored_bytes.is_some() {
            ignored_usage(workspace, &paths)?
        } else {
            0
        };
        Ok((paths.len(), bytes))
    }

    pub(super) fn enforce_result_policy(
        policy: &Policy,
        patch_bytes: u64,
        changed_paths: usize,
        ignored_paths: usize,
        ignored_bytes: u64,
    ) -> Result<()> {
        enforce_limit("patch bytes", patch_bytes, Some(policy.max_patch_bytes))?;
        enforce_limit(
            "changed paths",
            changed_paths as u64,
            policy.max_changed_paths,
        )?;
        enforce_limit(
            "ignored paths",
            ignored_paths as u64,
            policy.max_ignored_paths,
        )?;
        enforce_limit("ignored bytes", ignored_bytes, policy.max_ignored_bytes)
    }
}

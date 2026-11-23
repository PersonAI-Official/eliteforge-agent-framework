use super::*;
use std::{
    collections::BTreeMap,
    sync::{Mutex, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Clone)]
pub struct LocalFileCoordination {
    locks: Arc<Mutex<BTreeMap<String, Weak<AsyncMutex<()>>>>>,
    observations: Arc<Mutex<Observations>>,
    active: Arc<Mutex<BTreeMap<String, Vec<FileResource>>>>,
}
impl Default for LocalFileCoordination {
    fn default() -> Self {
        static SHARED: std::sync::OnceLock<LocalFileCoordination> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| Self {
                locks: Default::default(),
                observations: Default::default(),
                active: Default::default(),
            })
            .clone()
    }
}
#[derive(Default)]
struct Observations {
    clock: u64,
    records: BTreeMap<(FileSubject, String, FileResource), (u64, FileObservation)>,
}
struct LocalLease {
    resources: Vec<FileResource>,
    token: String,
    active: Arc<Mutex<BTreeMap<String, Vec<FileResource>>>>,
    guards: Mutex<Option<Vec<OwnedMutexGuard<()>>>>,
    locks: Vec<Arc<AsyncMutex<()>>>,
    released: tokio::sync::Notify,
}
#[async_trait]
impl FileCoordinationLease for LocalLease {
    fn resources(&self) -> &[FileResource] {
        &self.resources
    }
    fn ownership_token(&self) -> &str {
        &self.token
    }
    async fn validate(&self) -> Result<(), ToolError> {
        let _ = &self.locks; // Retain entries while waiting/holding, even across cancellation.
        if self
            .guards
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return Err(fail("file lease was released"));
        }
        Ok(())
    }
    async fn renew(&self) -> Result<(), ToolError> {
        self.validate().await
    }
    async fn lost(&self) {
        loop {
            let notified = self.released.notified();
            if self
                .guards
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return;
            }
            notified.await;
        }
    }
    async fn release(&self) -> Result<(), ToolError> {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
        self.guards.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.released.notify_waiters();
        Ok(())
    }
}
impl Drop for LocalLease {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
    }
}
impl LocalFileCoordination {
    fn owns(&self, lease: &dyn FileCoordinationLease) -> Result<(), ToolError> {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active
            .get(lease.ownership_token())
            .is_some_and(|r| r == lease.resources())
        {
            Ok(())
        } else {
            Err(fail("unknown or expired local file lease"))
        }
    }
}
fn fail(s: &str) -> ToolError {
    ToolError::Failed(s.into())
}
fn merge(existing: &mut FileObservation, next: FileObservation) {
    if existing.fingerprint != next.fingerprint {
        *existing = next;
        return;
    }
    existing.complete |= next.complete;
    for f in next.fragments {
        if !existing.fragments.contains(&f) {
            existing.fragments.push(f);
        }
    }
    existing.fragments.sort_by_key(|f| f.start_line);
    let mut through = 0;
    for f in &existing.fragments {
        if f.start_line > through + 1 {
            break;
        }
        through = through.max(f.end_line);
    }
    existing.complete |= through >= existing.total_lines;
}
impl Observations {
    fn store(&mut self, observation: FileObservation, accumulate: bool) {
        self.clock = self.clock.wrapping_add(1);
        let subject = observation.subject.clone();
        let key = (
            subject.clone(),
            observation.authorization_revision.clone(),
            observation.resource.clone(),
        );
        let record = self
            .records
            .entry(key)
            .or_insert((self.clock, observation.clone()));
        record.0 = self.clock;
        if accumulate {
            merge(&mut record.1, observation);
        } else {
            record.1 = observation;
        }
        while self.records.len() > 16_384
            || self
                .records
                .keys()
                .filter(|(s, _, _)| s == &subject)
                .count()
                > 4096
            || self
                .records
                .values()
                .map(|(_, o)| o.fragments.iter().map(|f| f.text.len()).sum::<usize>())
                .sum::<usize>()
                > 64 * 1024 * 1024
        {
            let subject_over = self
                .records
                .keys()
                .filter(|(s, _, _)| s == &subject)
                .count()
                > 4096;
            let key = self
                .records
                .iter()
                .filter(|((s, _, _), _)| !subject_over || s == &subject)
                .min_by_key(|(_, (tick, _))| *tick)
                .map(|(k, _)| k.clone());
            if let Some(key) = key {
                self.records.remove(&key);
            } else {
                break;
            }
        }
    }
}
#[async_trait]
impl FileCoordinationBackend for LocalFileCoordination {
    fn capabilities(&self) -> FileCoordinationCapabilities {
        FileCoordinationCapabilities {
            scope: FileCoordinationScope::Process,
            renewable_leases: false,
            storage_enforced_fencing: false,
        }
    }
    async fn acquire_many(
        &self,
        resources: Vec<FileResource>,
        run: &RunContext,
    ) -> Result<Arc<dyn FileCoordinationLease>, ToolError> {
        let locks = {
            let mut registry = self.locks.lock().unwrap_or_else(|e| e.into_inner());
            registry.retain(|_, lock| lock.strong_count() != 0);
            let keys: std::collections::BTreeSet<_> = resources
                .iter()
                .flat_map(|r| {
                    [
                        format!("identity:{}", r.key()),
                        format!("physical:{}", r.physical_alias),
                    ]
                })
                .collect();
            keys.into_iter()
                .map(|key| {
                    let lock = registry
                        .get(&key)
                        .and_then(Weak::upgrade)
                        .unwrap_or_else(|| Arc::new(AsyncMutex::new(())));
                    registry.insert(key, Arc::downgrade(&lock));
                    lock
                })
                .collect::<Vec<_>>()
        };
        let mut guards = Vec::new();
        for lock in &locks {
            let guard = tokio::select! {
                biased;
                _ = run.cancelled() => return Err(fail("cancelled waiting for file lease")),
                result = tokio::time::timeout_at(run.deadline, lock.clone().lock_owned()) => result.map_err(|_| fail("file lease deadline exceeded"))?,
            };
            guards.push(guard);
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token.clone(), resources.clone());
        Ok(Arc::new(LocalLease {
            resources,
            token,
            active: self.active.clone(),
            guards: Mutex::new(Some(guards)),
            locks,
            released: Default::default(),
        }))
    }
    async fn observations(
        &self,
        subject: &FileSubject,
        revision: &str,
        resources: &[FileResource],
    ) -> Result<Vec<FileObservation>, ToolError> {
        let mut store = self.observations.lock().unwrap_or_else(|e| e.into_inner());
        store.records.retain(|(s, rev, resource), _| {
            s != subject || rev == revision || !resources.contains(resource)
        });
        store.clock = store.clock.wrapping_add(1);
        let tick = store.clock;
        Ok(resources
            .iter()
            .filter_map(|r| {
                store
                    .records
                    .get_mut(&(subject.clone(), revision.into(), r.clone()))
                    .map(|(t, o)| {
                        *t = tick;
                        o.clone()
                    })
            })
            .collect())
    }
    async fn record_read(
        &self,
        lease: &dyn FileCoordinationLease,
        observation: FileObservation,
    ) -> Result<(), ToolError> {
        lease.validate().await?;
        self.owns(lease)?;
        if !lease.resources().contains(&observation.resource) {
            return Err(fail("observation exceeds file lease"));
        }
        self.observations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .store(observation, true);
        Ok(())
    }
    async fn publish(
        &self,
        lease: &dyn FileCoordinationLease,
        outcome: FileEditOutcome,
    ) -> Result<(), ToolError> {
        lease.validate().await?;
        self.owns(lease)?;
        if outcome
            .resources
            .iter()
            .chain(outcome.observations.iter().map(|o| &o.resource))
            .any(|r| !lease.resources().contains(r))
        {
            return Err(fail("outcome exceeds file lease"));
        }
        let mut store = self.observations.lock().unwrap_or_else(|e| e.into_inner());
        // Other subjects retain stale observations: contextual edits may still apply.
        store
            .records
            .retain(|(s, _, r), _| s != &outcome.subject || !outcome.resources.contains(r));
        for observation in outcome.observations {
            store.store(observation, false);
        }
        Ok(())
    }
    async fn invalidate(&self, invalidation: FileInvalidation) -> Result<(), ToolError> {
        self.observations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .retain(|(subject, revision, resource), _| {
                !((invalidation.subject.as_ref().is_none_or(|s| s == subject))
                    && invalidation
                        .session_id
                        .as_ref()
                        .is_none_or(|s| s == &subject.session_id)
                    && invalidation
                        .authorization_revision
                        .as_ref()
                        .is_none_or(|r| r == revision)
                    && (invalidation.resources.is_empty()
                        || invalidation.resources.contains(resource)))
            });
        Ok(())
    }
}

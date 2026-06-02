use std::collections::HashMap;

use dhttp::name::DhttpName;

use super::owner::Owner;
use crate::hypervisor::resource::{AsyncReleaseGuard, Completion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConflictReason {
    CrossOwnerAcquire,
}

#[derive(Debug)]
pub(super) enum ListenerSlot<R = ()> {
    Transition {
        owner: Owner,
        done: Completion,
    },
    Active {
        owner: Owner,
        resource: R,
        guard: AsyncReleaseGuard,
    },
    Poisoned {
        reason: ConflictReason,
    },
}

#[derive(Debug)]
pub(super) enum AcquirePlan<R = ()> {
    Build {
        done: Completion,
    },
    Wait(Completion),
    Duplicate,
    Conflict,
    DestroyConflict {
        owner: Owner,
        resource: R,
        guard: AsyncReleaseGuard,
        done: Completion,
    },
}

#[derive(Debug)]
pub(super) enum ReleasePlan<R = ()> {
    Destroy {
        resource: R,
        guard: AsyncReleaseGuard,
        done: Completion,
    },
    Wait(Completion),
    NotOwner,
    NotFound,
    StaleHandle,
    Poisoned,
}

#[derive(Debug)]
pub(super) enum RebuildPlan<R = ()> {
    Rebuild {
        resource: R,
        guard: AsyncReleaseGuard,
        done: Completion,
    },
    Wait(Completion),
    NotOwner,
    NotFound,
    Conflict,
}

#[derive(Debug)]
pub(super) struct ListenerRegistry<R = ()> {
    entries: HashMap<DhttpName<'static>, ListenerSlot<R>>,
}

impl<R> ListenerRegistry<R> {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn entry(&self, name: &DhttpName<'static>) -> Option<&ListenerSlot<R>> {
        self.entries.get(name)
    }

    #[cfg(test)]
    pub(super) fn contains(&self, name: &DhttpName<'static>) -> bool {
        self.entries.contains_key(name)
    }

    #[cfg(test)]
    pub(super) fn is_active(&self, name: &DhttpName<'static>) -> bool {
        matches!(self.entries.get(name), Some(ListenerSlot::Active { .. }))
    }

    pub(super) fn plan_acquire(
        &mut self,
        owner: Owner,
        name: DhttpName<'static>,
    ) -> AcquirePlan<R> {
        match self.entries.remove(&name) {
            None => {
                let done = Completion::new();
                self.entries.insert(
                    name,
                    ListenerSlot::Transition {
                        owner,
                        done: done.clone(),
                    },
                );
                AcquirePlan::Build { done }
            }
            Some(ListenerSlot::Transition {
                owner: existing_owner,
                done,
            }) => {
                self.entries.insert(
                    name,
                    ListenerSlot::Transition {
                        owner: existing_owner,
                        done: done.clone(),
                    },
                );
                AcquirePlan::Wait(done)
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
                guard,
            }) if existing_owner == owner => {
                self.entries.insert(
                    name,
                    ListenerSlot::Active {
                        owner: existing_owner,
                        resource,
                        guard,
                    },
                );
                AcquirePlan::Duplicate
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
                guard,
            }) => {
                let done = Completion::new();
                self.entries.insert(
                    name,
                    ListenerSlot::Transition {
                        owner: existing_owner,
                        done: done.clone(),
                    },
                );
                AcquirePlan::DestroyConflict {
                    owner: existing_owner,
                    resource,
                    guard,
                    done,
                }
            }
            Some(ListenerSlot::Poisoned { reason }) => {
                self.entries.insert(name, ListenerSlot::Poisoned { reason });
                AcquirePlan::Conflict
            }
        }
    }

    pub(super) fn commit_transition_active(
        &mut self,
        owner: Owner,
        name: DhttpName<'static>,
        done: &Completion,
        resource: R,
        guard: AsyncReleaseGuard,
    ) -> Result<(), R> {
        let matches_slot = matches!(
            self.entries.get(&name),
            Some(ListenerSlot::Transition {
                owner: existing_owner,
                done: existing_done,
            }) if *existing_owner == owner && existing_done.ptr_eq(done)
        );

        if matches_slot {
            self.entries.insert(
                name,
                ListenerSlot::Active {
                    owner,
                    resource,
                    guard,
                },
            );
            done.complete();
            Ok(())
        } else {
            Err(resource)
        }
    }

    pub(super) fn finish_transition_vacant(
        &mut self,
        owner: Owner,
        name: &DhttpName<'static>,
        done: &Completion,
    ) -> bool {
        let matches_slot = matches!(
            self.entries.get(name),
            Some(ListenerSlot::Transition {
                owner: existing_owner,
                done: existing_done,
            }) if *existing_owner == owner && existing_done.ptr_eq(done)
        );

        if matches_slot {
            self.entries.remove(name);
            done.complete();
            true
        } else {
            false
        }
    }

    pub(super) fn finish_transition_poisoned(
        &mut self,
        owner: Owner,
        name: &DhttpName<'static>,
        done: &Completion,
    ) -> bool {
        let matches_slot = matches!(
            self.entries.get(name),
            Some(ListenerSlot::Transition {
                owner: existing_owner,
                done: existing_done,
            }) if *existing_owner == owner && existing_done.ptr_eq(done)
        );

        if matches_slot {
            self.entries.insert(
                name.clone(),
                ListenerSlot::Poisoned {
                    reason: ConflictReason::CrossOwnerAcquire,
                },
            );
            done.complete();
            true
        } else {
            false
        }
    }

    pub(super) fn plan_release(
        &mut self,
        owner: Owner,
        name: &DhttpName<'static>,
        expected_guard: Option<&AsyncReleaseGuard>,
    ) -> ReleasePlan<R> {
        match self.entries.remove(name) {
            None => ReleasePlan::NotFound,
            Some(ListenerSlot::Transition {
                owner: existing_owner,
                done,
            }) => {
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Transition {
                        owner: existing_owner,
                        done: done.clone(),
                    },
                );
                ReleasePlan::Wait(done)
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
                guard,
            }) if existing_owner == owner
                && expected_guard.is_none_or(|expected| guard.ptr_eq(expected)) =>
            {
                let done = Completion::new();
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Transition {
                        owner: existing_owner,
                        done: done.clone(),
                    },
                );
                ReleasePlan::Destroy {
                    resource,
                    guard,
                    done,
                }
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
                guard,
            }) if existing_owner == owner => {
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Active {
                        owner: existing_owner,
                        resource,
                        guard,
                    },
                );
                ReleasePlan::StaleHandle
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
                guard,
            }) => {
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Active {
                        owner: existing_owner,
                        resource,
                        guard,
                    },
                );
                ReleasePlan::NotOwner
            }
            Some(ListenerSlot::Poisoned { reason }) => {
                self.entries
                    .insert(name.clone(), ListenerSlot::Poisoned { reason });
                ReleasePlan::Poisoned
            }
        }
    }

    pub(super) fn plan_rebuild(
        &mut self,
        owner: Owner,
        name: &DhttpName<'static>,
    ) -> RebuildPlan<R> {
        match self.entries.remove(name) {
            None => RebuildPlan::NotFound,
            Some(ListenerSlot::Transition {
                owner: existing_owner,
                done,
            }) => {
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Transition {
                        owner: existing_owner,
                        done: done.clone(),
                    },
                );
                RebuildPlan::Wait(done)
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
                guard,
            }) if existing_owner == owner => {
                let done = Completion::new();
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Transition {
                        owner: existing_owner,
                        done: done.clone(),
                    },
                );
                RebuildPlan::Rebuild {
                    resource,
                    guard,
                    done,
                }
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
                guard,
            }) => {
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Active {
                        owner: existing_owner,
                        resource,
                        guard,
                    },
                );
                RebuildPlan::NotOwner
            }
            Some(ListenerSlot::Poisoned { reason }) => {
                self.entries
                    .insert(name.clone(), ListenerSlot::Poisoned { reason });
                RebuildPlan::Conflict
            }
        }
    }

    pub(super) fn clear_poisoned(&mut self) -> Vec<DhttpName<'static>> {
        let poisoned = self
            .entries
            .iter()
            .filter_map(|(name, slot)| {
                matches!(slot, ListenerSlot::Poisoned { .. }).then_some(name.clone())
            })
            .collect::<Vec<_>>();

        for name in &poisoned {
            self.entries.remove(name);
        }

        poisoned
    }

    pub(super) fn owned_names(&self, owner: Owner) -> Vec<DhttpName<'static>> {
        self.entries
            .iter()
            .filter_map(|(name, slot)| match slot {
                ListenerSlot::Transition {
                    owner: existing_owner,
                    ..
                }
                | ListenerSlot::Active {
                    owner: existing_owner,
                    ..
                } if *existing_owner == owner => Some(name.clone()),
                ListenerSlot::Transition { .. }
                | ListenerSlot::Active { .. }
                | ListenerSlot::Poisoned { .. } => None,
            })
            .collect()
    }
}

impl<R> Default for ListenerRegistry<R> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl ListenerRegistry<()> {
    fn insert_active_for_test(
        &mut self,
        name: DhttpName<'static>,
        owner: Owner,
        guard: AsyncReleaseGuard,
    ) {
        self.entries.insert(
            name,
            ListenerSlot::Active {
                owner,
                resource: (),
                guard,
            },
        );
    }

    fn insert_transition_for_test(
        &mut self,
        name: DhttpName<'static>,
        owner: Owner,
        done: Completion,
    ) {
        self.entries
            .insert(name, ListenerSlot::Transition { owner, done });
    }

    fn insert_poisoned_for_test(&mut self, name: DhttpName<'static>) {
        self.entries.insert(
            name,
            ListenerSlot::Poisoned {
                reason: ConflictReason::CrossOwnerAcquire,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use dhttp::name::DhttpName;
    use nix::unistd::{Pid, Uid};

    use super::*;
    use crate::hypervisor::state::owner::Owner;

    fn name(value: &str) -> DhttpName<'static> {
        DhttpName::try_from(value.to_owned()).expect("valid dhttp name")
    }

    fn worker(pid: i32) -> Owner {
        Owner::Worker {
            uid: Uid::from_raw(1000),
            pid: Pid::from_raw(pid),
        }
    }

    #[test]
    fn vacant_acquire_inserts_transition() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let name = name("alpha.user.genmeta.net");
        let plan = registry.plan_acquire(worker(10), name.clone());

        assert!(matches!(plan, AcquirePlan::Build { .. }));
        assert!(matches!(
            registry.entry(&name),
            Some(ListenerSlot::Transition { owner, .. }) if *owner == worker(10)
        ));
    }

    #[test]
    fn cross_owner_acquire_transitions_before_poison() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let name = name("alpha.user.genmeta.net");
        registry.insert_active_for_test(name.clone(), worker(10), AsyncReleaseGuard::new());

        let plan = registry.plan_acquire(worker(20), name.clone());

        assert!(matches!(plan, AcquirePlan::DestroyConflict { .. }));
        assert!(matches!(
            registry.entry(&name),
            Some(ListenerSlot::Transition { owner, .. }) if *owner == worker(10)
        ));
    }

    #[tokio::test]
    async fn transition_waiter_retries_after_completion() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let name = name("alpha.user.genmeta.net");
        let done = Completion::new();
        registry.insert_transition_for_test(name.clone(), worker(10), done.clone());

        let plan = registry.plan_acquire(worker(10), name.clone());
        let AcquirePlan::Wait(wait) = plan else {
            panic!("expected wait plan");
        };

        registry.finish_transition_vacant(worker(10), &name, &done);
        wait.wait().await;
        assert!(matches!(
            registry.plan_acquire(worker(10), name),
            AcquirePlan::Build { .. }
        ));
    }

    #[test]
    fn stale_handle_release_does_not_remove_replacement() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let name = name("alpha.user.genmeta.net");
        let old_guard = AsyncReleaseGuard::new();
        let new_guard = AsyncReleaseGuard::new();
        registry.insert_active_for_test(name.clone(), worker(10), new_guard.clone());

        let plan = registry.plan_release(worker(10), &name, Some(&old_guard));

        assert!(matches!(plan, ReleasePlan::StaleHandle));
        assert!(matches!(
            registry.entry(&name),
            Some(ListenerSlot::Active { guard, .. }) if guard.ptr_eq(&new_guard)
        ));
    }

    #[test]
    fn poison_clear_removes_only_poisoned_slots() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let poisoned = name("poisoned.user.genmeta.net");
        let active = name("active.user.genmeta.net");
        registry.insert_poisoned_for_test(poisoned.clone());
        registry.insert_active_for_test(active.clone(), worker(10), AsyncReleaseGuard::new());

        let cleared = registry.clear_poisoned();

        assert_eq!(cleared, vec![poisoned.clone()]);
        assert!(registry.entry(&poisoned).is_none());
        assert!(matches!(
            registry.entry(&active),
            Some(ListenerSlot::Active { .. })
        ));
    }
}

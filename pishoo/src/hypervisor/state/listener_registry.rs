#![allow(dead_code)]

use std::collections::HashMap;

use dhttp::name::DhttpName;

use super::{completion::Completion, owner::Owner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DestroyReason {
    Release,
    Rebuild,
    Conflict,
    OwnerDead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConflictReason {
    CrossOwnerAcquire,
}

#[derive(Debug)]
pub(super) enum ListenerSlot<R = ()> {
    Creating {
        owner: Owner,
        done: Completion,
    },
    Active {
        owner: Owner,
        resource: R,
    },
    Destroying {
        owner: Owner,
        reason: DestroyReason,
        done: Completion,
    },
    Poisoned {
        reason: ConflictReason,
    },
}

#[derive(Debug)]
pub(super) enum AcquirePlan<R = ()> {
    Build { done: Completion },
    Wait(Completion),
    Duplicate,
    Conflict,
    DestroyConflict { resource: R, done: Completion },
}

#[derive(Debug)]
pub(super) enum DestroyFinish {
    Vacant,
    Poisoned,
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

    pub(super) fn entry(&self, name: &DhttpName<'static>) -> Option<&ListenerSlot<R>> {
        self.entries.get(name)
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
                    ListenerSlot::Creating {
                        owner,
                        done: done.clone(),
                    },
                );
                AcquirePlan::Build { done }
            }
            Some(ListenerSlot::Creating {
                owner: existing_owner,
                done,
            }) => {
                self.entries.insert(
                    name,
                    ListenerSlot::Creating {
                        owner: existing_owner,
                        done: done.clone(),
                    },
                );
                AcquirePlan::Wait(done)
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
            }) if existing_owner == owner => {
                self.entries.insert(
                    name,
                    ListenerSlot::Active {
                        owner: existing_owner,
                        resource,
                    },
                );
                AcquirePlan::Duplicate
            }
            Some(ListenerSlot::Active {
                owner: existing_owner,
                resource,
            }) => {
                let done = Completion::new();
                self.entries.insert(
                    name,
                    ListenerSlot::Destroying {
                        owner: existing_owner,
                        reason: DestroyReason::Conflict,
                        done: done.clone(),
                    },
                );
                AcquirePlan::DestroyConflict { resource, done }
            }
            Some(ListenerSlot::Destroying {
                owner: existing_owner,
                reason,
                done,
            }) => {
                self.entries.insert(
                    name,
                    ListenerSlot::Destroying {
                        owner: existing_owner,
                        reason,
                        done: done.clone(),
                    },
                );
                AcquirePlan::Wait(done)
            }
            Some(ListenerSlot::Poisoned { reason }) => {
                self.entries.insert(name, ListenerSlot::Poisoned { reason });
                AcquirePlan::Conflict
            }
        }
    }

    pub(super) fn finish_destroying(
        &mut self,
        name: &DhttpName<'static>,
        done: &Completion,
        finish: DestroyFinish,
    ) {
        let matches_slot = matches!(
            self.entries.get(name),
            Some(ListenerSlot::Destroying {
                done: existing_done,
                ..
            }) if existing_done.ptr_eq(done)
        );

        if !matches_slot {
            return;
        }

        match finish {
            DestroyFinish::Vacant => {
                self.entries.remove(name);
            }
            DestroyFinish::Poisoned => {
                self.entries.insert(
                    name.clone(),
                    ListenerSlot::Poisoned {
                        reason: ConflictReason::CrossOwnerAcquire,
                    },
                );
            }
        }
        done.complete();
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
}

impl<R> Default for ListenerRegistry<R> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl ListenerRegistry<()> {
    fn insert_active_for_test(&mut self, name: DhttpName<'static>, owner: Owner) {
        self.entries.insert(
            name,
            ListenerSlot::Active {
                owner,
                resource: (),
            },
        );
    }

    fn insert_destroying_for_test(
        &mut self,
        name: DhttpName<'static>,
        owner: Owner,
        reason: DestroyReason,
        done: Completion,
    ) {
        self.entries.insert(
            name,
            ListenerSlot::Destroying {
                owner,
                reason,
                done,
            },
        );
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
    fn vacant_acquire_inserts_creating() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let name = name("alpha.user.genmeta.net");
        let plan = registry.plan_acquire(worker(10), name.clone());

        assert!(matches!(plan, AcquirePlan::Build { .. }));
        assert!(matches!(
            registry.entry(&name),
            Some(ListenerSlot::Creating { owner, .. }) if *owner == worker(10)
        ));
    }

    #[test]
    fn cross_owner_acquire_retires_to_destroying_before_poison() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let name = name("alpha.user.genmeta.net");
        registry.insert_active_for_test(name.clone(), worker(10));

        let plan = registry.plan_acquire(worker(20), name.clone());

        assert!(matches!(plan, AcquirePlan::DestroyConflict { .. }));
        assert!(matches!(
            registry.entry(&name),
            Some(ListenerSlot::Destroying { owner, reason: DestroyReason::Conflict, .. })
                if *owner == worker(10)
        ));
    }

    #[tokio::test]
    async fn destroying_waiter_retries_after_completion() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let name = name("alpha.user.genmeta.net");
        let done = Completion::new();
        registry.insert_destroying_for_test(
            name.clone(),
            worker(10),
            DestroyReason::Release,
            done.clone(),
        );

        let plan = registry.plan_acquire(worker(10), name.clone());
        let AcquirePlan::Wait(wait) = plan else {
            panic!("expected wait plan");
        };

        registry.finish_destroying(&name, &done, DestroyFinish::Vacant);
        wait.wait().await;
        assert!(matches!(
            registry.plan_acquire(worker(10), name),
            AcquirePlan::Build { .. }
        ));
    }

    #[test]
    fn poison_clear_removes_only_poisoned_slots() {
        let mut registry: ListenerRegistry = ListenerRegistry::new();
        let poisoned = name("poisoned.user.genmeta.net");
        let active = name("active.user.genmeta.net");
        registry.insert_poisoned_for_test(poisoned.clone());
        registry.insert_active_for_test(active.clone(), worker(10));

        let cleared = registry.clear_poisoned();

        assert_eq!(cleared, vec![poisoned.clone()]);
        assert!(registry.entry(&poisoned).is_none());
        assert!(matches!(
            registry.entry(&active),
            Some(ListenerSlot::Active { .. })
        ));
    }
}

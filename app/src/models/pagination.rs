use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PaginationScope {
    Discover,
    Library,
    ContinueWatching,
    Addons,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PaginationIdentity {
    scope: PaginationScope,
    selected: [u8; 32],
    next: [u8; 32],
}

impl PaginationIdentity {
    pub(crate) fn new<S, N>(scope: PaginationScope, selected: Option<&S>, next: &N) -> Option<Self>
    where
        S: Serialize + ?Sized,
        N: Serialize + ?Sized,
    {
        Some(Self {
            scope,
            selected: fingerprint(&selected)?,
            next: fingerprint(next)?,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct GateState {
    identity: PaginationIdentity,
    pending: bool,
}

#[derive(Default)]
pub(crate) struct PaginationGate {
    states: Mutex<HashMap<PaginationScope, GateState>>,
}

impl PaginationGate {
    pub(crate) fn observe(&self, scope: PaginationScope, identity: Option<PaginationIdentity>) {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(identity) = identity else {
            states.remove(&scope);
            return;
        };
        match states.get_mut(&scope) {
            Some(state) if state.identity == identity => {}
            Some(state) => {
                *state = GateState {
                    identity,
                    pending: false,
                }
            }
            None => {
                states.insert(
                    scope,
                    GateState {
                        identity,
                        pending: false,
                    },
                );
            }
        }
    }

    pub(crate) fn try_begin(&self, identity: Option<PaginationIdentity>) -> bool {
        let Some(identity) = identity else {
            return false;
        };
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(identity.scope).or_insert(GateState {
            identity,
            pending: false,
        });
        if state.identity != identity {
            *state = GateState {
                identity,
                pending: false,
            };
        }
        if state.pending {
            return false;
        }
        state.pending = true;
        true
    }

    pub(crate) fn reset(&self, scope: PaginationScope) {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&scope);
    }
}

pub(crate) fn gate() -> &'static PaginationGate {
    static GATE: OnceLock<PaginationGate> = OnceLock::new();
    GATE.get_or_init(PaginationGate::default)
}

fn fingerprint(value: &(impl Serialize + ?Sized)) -> Option<[u8; 32]> {
    let encoded = serde_json::to_vec(value).ok()?;
    Some(*blake3::hash(&encoded).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{PaginationGate, PaginationIdentity, PaginationScope};

    fn identity(selected: &str, next: usize) -> PaginationIdentity {
        PaginationIdentity::new(PaginationScope::Discover, Some(selected), &next)
            .expect("serializable pagination identity")
    }

    #[test]
    fn repeated_scroll_events_dispatch_only_once() {
        let gate = PaginationGate::default();
        let identity = identity("catalog-a", 2);

        assert!(gate.try_begin(Some(identity)));
        assert!(!gate.try_begin(Some(identity)));
    }

    #[test]
    fn different_next_page_rearms_the_gate() {
        let gate = PaginationGate::default();
        assert!(gate.try_begin(Some(identity("catalog-a", 2))));

        gate.observe(PaginationScope::Discover, Some(identity("catalog-a", 3)));

        assert!(gate.try_begin(Some(identity("catalog-a", 3))));
    }

    #[test]
    fn selected_request_change_resets_pending_state() {
        let gate = PaginationGate::default();
        assert!(gate.try_begin(Some(identity("catalog-a", 2))));

        gate.observe(PaginationScope::Discover, Some(identity("catalog-b", 2)));

        assert!(gate.try_begin(Some(identity("catalog-b", 2))));
    }

    #[test]
    fn missing_next_page_cannot_dispatch() {
        let gate = PaginationGate::default();

        assert!(!gate.try_begin(None));
    }
}

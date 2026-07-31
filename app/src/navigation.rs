use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::MainWindow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tab {
    Board,
    Discover,
    Library,
    Addons,
    Settings,
    Calendar,
    Downloads,
    Movies,
    Shows,
    Anime,
    Kids,
}

impl Tab {
    pub const fn index(self) -> i32 {
        match self {
            Self::Board => 0,
            Self::Discover => 1,
            Self::Library => 2,
            Self::Addons => 3,
            Self::Settings => 4,
            Self::Calendar => 5,
            Self::Downloads => 7,
            Self::Movies => 8,
            Self::Shows => 9,
            Self::Anime => 10,
            Self::Kids => 11,
        }
    }

    pub const fn is_discovery(self) -> bool {
        matches!(
            self,
            Self::Discover | Self::Movies | Self::Shows | Self::Anime | Self::Kids
        )
    }
}

impl TryFrom<i32> for Tab {
    type Error = i32;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Board),
            1 => Ok(Self::Discover),
            2 => Ok(Self::Library),
            3 => Ok(Self::Addons),
            4 => Ok(Self::Settings),
            5 => Ok(Self::Calendar),
            7 => Ok(Self::Downloads),
            8 => Ok(Self::Movies),
            9 => Ok(Self::Shows),
            10 => Ok(Self::Anime),
            11 => Ok(Self::Kids),
            invalid => Err(invalid),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetailsPresentation {
    Preview,
    Full,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    Tab(Tab),
    Search { query: String },
    AddonDetails { transport_url: String },
    Details { media_id: String },
    Player,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavigationIntent {
    SelectTab(Tab),
    OpenSearch { query: String },
    SelectDiscoverPreview { media_id: String },
    OpenAddonDetails { transport_url: String },
    OpenDetails { media_id: String },
    OpenPlayer,
    Back,
    Forward,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NavigationHistoryEntry {
    routes: Vec<Route>,
    discover_preview_id: Option<String>,
}

impl From<&NavigationSnapshot> for NavigationHistoryEntry {
    fn from(snapshot: &NavigationSnapshot) -> Self {
        Self {
            routes: snapshot.routes.clone(),
            discover_preview_id: snapshot.discover_preview_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationSnapshot {
    pub routes: Vec<Route>,
    pub revision: u64,
    pub discover_preview_id: Option<String>,
}

impl NavigationSnapshot {
    pub fn active_tab_index(&self) -> i32 {
        if self
            .routes
            .iter()
            .any(|route| matches!(route, Route::Search { .. }))
        {
            6
        } else {
            self.root_tab().index()
        }
    }

    pub fn root_tab(&self) -> Tab {
        match self.routes.first() {
            Some(Route::Tab(tab)) => *tab,
            _ => Tab::Board,
        }
    }

    pub fn show_details(&self) -> bool {
        self.routes
            .iter()
            .any(|route| matches!(route, Route::Details { .. }))
    }

    pub fn show_player(&self) -> bool {
        matches!(self.routes.last(), Some(Route::Player))
    }

    pub fn show_addon_details(&self) -> bool {
        matches!(self.routes.last(), Some(Route::AddonDetails { .. }))
    }

    fn details_presentation(&self, media_id: &str) -> Option<DetailsPresentation> {
        if self.routes.iter().rev().any(
            |route| matches!(route, Route::Details { media_id: expected } if expected == media_id),
        ) {
            return Some(DetailsPresentation::Full);
        }

        (self.root_tab().is_discovery() && self.discover_preview_id.as_deref() == Some(media_id))
            .then_some(DetailsPresentation::Preview)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationTransition {
    pub before: NavigationSnapshot,
    pub after: NavigationSnapshot,
    pub changed: bool,
}

#[derive(Clone)]
pub struct NavigationController {
    state: Arc<RwLock<NavigationSnapshot>>,
    forward_history: Arc<Mutex<Vec<NavigationHistoryEntry>>>,
}

impl NavigationController {
    pub fn new(initial_tab: i32) -> Self {
        let tab = Tab::try_from(initial_tab).unwrap_or_else(|invalid| {
            tracing::warn!(invalid, "invalid configured tab; falling back to Board");
            Tab::Board
        });
        Self {
            state: Arc::new(RwLock::new(NavigationSnapshot {
                routes: vec![Route::Tab(tab)],
                revision: 0,
                discover_preview_id: None,
            })),
            forward_history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn snapshot(&self) -> NavigationSnapshot {
        self.read_state().clone()
    }

    pub fn active_tab_index(&self) -> i32 {
        self.read_state().active_tab_index()
    }

    pub fn is_player_visible(&self) -> bool {
        self.read_state().show_player()
    }

    pub fn details_presentation(&self, media_id: &str) -> Option<DetailsPresentation> {
        self.read_state().details_presentation(media_id)
    }

    pub fn dispatch(&self, intent: NavigationIntent) -> NavigationTransition {
        let mut state = self.write_state();
        let before = state.clone();
        let changed = match &intent {
            NavigationIntent::Back => {
                let history_entry = NavigationHistoryEntry::from(&*state);
                let changed = reduce(&mut state, intent.clone());
                if changed {
                    self.lock_forward_history().push(history_entry);
                }
                changed
            }
            NavigationIntent::Forward => {
                let Some(history_entry) = self.lock_forward_history().pop() else {
                    return NavigationTransition {
                        before: before.clone(),
                        after: before,
                        changed: false,
                    };
                };
                state.routes = history_entry.routes;
                state.discover_preview_id = history_entry.discover_preview_id;
                true
            }
            _ => {
                let changed = reduce(&mut state, intent.clone());
                if changed {
                    self.lock_forward_history().clear();
                }
                changed
            }
        };
        if changed {
            state.revision = state.revision.wrapping_add(1);
        }
        let after = state.clone();
        drop(state);

        if changed {
            tracing::info!(
                ?intent,
                from = ?before.routes,
                to = ?after.routes,
                revision = after.revision,
                "navigation transition"
            );
        }

        NavigationTransition {
            before,
            after,
            changed,
        }
    }

    pub fn dispatch_and_project(
        &self,
        ui: &MainWindow,
        intent: NavigationIntent,
    ) -> NavigationTransition {
        let transition = self.dispatch(intent);
        Self::project_snapshot(ui, &transition.after);
        transition
    }

    pub fn project(&self, ui: &MainWindow) {
        Self::project_snapshot(ui, &self.snapshot());
    }

    fn project_snapshot(ui: &MainWindow, state: &NavigationSnapshot) {
        ui.set_active_tab(state.active_tab_index());
        ui.set_show_details(state.show_details());
        ui.set_show_player(state.show_player());
        ui.set_addon_details_open(state.show_addon_details());
    }

    fn read_state(&self) -> RwLockReadGuard<'_, NavigationSnapshot> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_state(&self) -> RwLockWriteGuard<'_, NavigationSnapshot> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_forward_history(&self) -> std::sync::MutexGuard<'_, Vec<NavigationHistoryEntry>> {
        self.forward_history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn reduce(state: &mut NavigationSnapshot, intent: NavigationIntent) -> bool {
    match intent {
        NavigationIntent::SelectTab(tab) => {
            let routes = vec![Route::Tab(tab)];
            let preview = tab
                .is_discovery()
                .then(|| state.discover_preview_id.clone())
                .flatten();
            if state.routes == routes && state.discover_preview_id == preview {
                return false;
            }
            state.routes = routes;
            state.discover_preview_id = preview;
            true
        }
        NavigationIntent::OpenSearch { query } => {
            state.routes.truncate(usize::from(!state.routes.is_empty()));
            state.routes.push(Route::Search { query });
            state.discover_preview_id = None;
            true
        }
        NavigationIntent::SelectDiscoverPreview { media_id } => {
            if !state.root_tab().is_discovery()
                || state.discover_preview_id.as_deref() == Some(media_id.as_str())
            {
                return false;
            }
            state.discover_preview_id = Some(media_id);
            true
        }
        NavigationIntent::OpenAddonDetails { transport_url } => {
            if state.active_tab_index() != Tab::Addons.index() {
                return false;
            }
            while matches!(state.routes.last(), Some(Route::AddonDetails { .. })) {
                state.routes.pop();
            }
            state.routes.push(Route::AddonDetails { transport_url });
            true
        }
        NavigationIntent::OpenDetails { media_id } => {
            while matches!(
                state.routes.last(),
                Some(Route::Player | Route::Details { .. })
            ) {
                state.routes.pop();
            }
            state.routes.push(Route::Details { media_id });
            true
        }
        NavigationIntent::OpenPlayer => {
            if state.show_player() {
                return false;
            }
            state.routes.push(Route::Player);
            true
        }
        NavigationIntent::Back => {
            if state.routes.len() <= 1 {
                return false;
            }
            state.routes.pop();
            true
        }
        NavigationIntent::Forward => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_initial_tab_falls_back_to_board() {
        let navigation = NavigationController::new(99);

        assert_eq!(navigation.active_tab_index(), Tab::Board.index());
    }

    #[test]
    fn back_from_player_reveals_the_existing_details_route() {
        let navigation = NavigationController::new(Tab::Board.index());
        navigation.dispatch(NavigationIntent::OpenDetails {
            media_id: "movie-a".to_owned(),
        });
        navigation.dispatch(NavigationIntent::OpenPlayer);

        let transition = navigation.dispatch(NavigationIntent::Back);

        assert_eq!(
            transition.after.routes,
            vec![
                Route::Tab(Tab::Board),
                Route::Details {
                    media_id: "movie-a".to_owned()
                }
            ]
        );
    }

    #[test]
    fn selecting_a_tab_invalidates_an_open_details_request() {
        let navigation = NavigationController::new(Tab::Board.index());
        navigation.dispatch(NavigationIntent::OpenDetails {
            media_id: "movie-a".to_owned(),
        });

        navigation.dispatch(NavigationIntent::SelectTab(Tab::Settings));

        assert_eq!(navigation.details_presentation("movie-a"), None);
    }

    #[test]
    fn rapid_details_navigation_only_accepts_the_latest_request() {
        let navigation = NavigationController::new(Tab::Board.index());
        navigation.dispatch(NavigationIntent::OpenDetails {
            media_id: "movie-a".to_owned(),
        });
        navigation.dispatch(NavigationIntent::OpenDetails {
            media_id: "series-b".to_owned(),
        });

        assert_eq!(navigation.details_presentation("movie-a"), None);
        assert_eq!(
            navigation.details_presentation("series-b"),
            Some(DetailsPresentation::Full)
        );
        assert_eq!(
            navigation.snapshot().routes,
            vec![
                Route::Tab(Tab::Board),
                Route::Details {
                    media_id: "series-b".to_owned()
                }
            ]
        );
    }

    #[test]
    fn only_the_latest_discover_preview_accepts_metadata() {
        let navigation = NavigationController::new(Tab::Discover.index());
        navigation.dispatch(NavigationIntent::SelectDiscoverPreview {
            media_id: "movie-a".to_owned(),
        });
        navigation.dispatch(NavigationIntent::SelectDiscoverPreview {
            media_id: "movie-b".to_owned(),
        });

        assert_eq!(
            navigation.details_presentation("movie-b"),
            Some(DetailsPresentation::Preview)
        );
    }

    #[test]
    fn back_from_search_returns_to_the_originating_tab() {
        let navigation = NavigationController::new(Tab::Library.index());
        navigation.dispatch(NavigationIntent::OpenSearch {
            query: "dune".to_owned(),
        });

        navigation.dispatch(NavigationIntent::Back);

        assert_eq!(navigation.active_tab_index(), Tab::Library.index());
    }

    #[test]
    fn forward_restores_the_route_removed_by_back() {
        let navigation = NavigationController::new(Tab::Board.index());
        navigation.dispatch(NavigationIntent::OpenDetails {
            media_id: "movie-a".to_owned(),
        });
        let expected = navigation.snapshot().routes;

        navigation.dispatch(NavigationIntent::Back);
        let transition = navigation.dispatch(NavigationIntent::Forward);

        assert!(transition.changed);
        assert_eq!(transition.after.routes, expected);
    }

    #[test]
    fn new_navigation_clears_forward_history() {
        let navigation = NavigationController::new(Tab::Board.index());
        navigation.dispatch(NavigationIntent::OpenDetails {
            media_id: "movie-a".to_owned(),
        });
        navigation.dispatch(NavigationIntent::Back);
        navigation.dispatch(NavigationIntent::SelectTab(Tab::Library));

        let transition = navigation.dispatch(NavigationIntent::Forward);

        assert!(!transition.changed);
        assert_eq!(transition.after.routes, vec![Route::Tab(Tab::Library)]);
    }

    #[test]
    fn closing_player_invalidates_its_pending_route_revision() {
        let navigation = NavigationController::new(Tab::Board.index());
        navigation.dispatch(NavigationIntent::OpenDetails {
            media_id: "movie-a".to_owned(),
        });
        let opened = navigation.dispatch(NavigationIntent::OpenPlayer);

        let closed = navigation.dispatch(NavigationIntent::Back);

        assert!(!navigation.is_player_visible());
        assert!(closed.after.revision > opened.after.revision);
    }

    #[test]
    fn addon_details_are_scoped_to_the_addons_tab() {
        let navigation = NavigationController::new(Tab::Board.index());
        let ignored = navigation.dispatch(NavigationIntent::OpenAddonDetails {
            transport_url: "https://example.com/manifest.json".to_owned(),
        });
        assert!(!ignored.changed);

        navigation.dispatch(NavigationIntent::SelectTab(Tab::Addons));
        navigation.dispatch(NavigationIntent::OpenAddonDetails {
            transport_url: "https://example.com/manifest.json".to_owned(),
        });
        assert!(navigation.snapshot().show_addon_details());

        navigation.dispatch(NavigationIntent::Back);
        assert!(!navigation.snapshot().show_addon_details());
    }
}

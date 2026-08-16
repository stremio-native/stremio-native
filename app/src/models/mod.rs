pub mod addons;
pub mod auth;
pub mod board;
pub mod calendar;
pub mod details;
pub mod discover;
pub mod events;
pub mod library;
pub mod onboarding;
pub mod pagination;
pub mod search;
pub mod settings;

use crate::{
    AddonItem, AppModel, AppModelField, BoardSection, CalendarMediaItem, EpisodeItem, MainWindow,
    MediaCardItem, SearchSuggestion,
};
use core_env::DesktopEnv;
use slint::{ComponentHandle, Model, ModelRc};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex, OnceLock},
};
use stremio_core::{
    models::{
        catalogs_with_extra::{Catalog, CatalogsWithExtra},
        common::{Loadable, ResourceError},
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionCatalogsWithExtra},
    },
    types::{addon::Descriptor, resource::MetaItemPreview},
};

const CATALOG_PREVIEW_SIZE: usize = 10;
const CATALOG_PRELOAD_ROWS: usize = 5;
const CATALOG_VISIBILITY_QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) enum CatalogScope {
    Board,
    Search,
}

impl CatalogScope {
    fn catalogs(self, model: &AppModel) -> &CatalogsWithExtra {
        match self {
            Self::Board => &model.board,
            Self::Search => &model.search,
        }
    }

    fn field(self) -> AppModelField {
        match self {
            Self::Board => AppModelField::Board,
            Self::Search => AppModelField::Search,
        }
    }
}

pub(crate) type SyncFingerprint = [u8; 32];

pub(crate) struct Fingerprint(blake3::Hasher);

impl Fingerprint {
    pub(crate) fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.usize(value.len());
        self.0.update(value.as_bytes());
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.usize(value.len());
        self.0.update(value);
    }

    pub(crate) fn optional_str(&mut self, value: Option<&str>) {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.str(value);
        }
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.0.update(&[u8::from(value)]);
    }

    pub(crate) fn usize(&mut self, value: usize) {
        self.0.update(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.0.update(&value.to_le_bytes());
    }

    pub(crate) fn finish(self) -> SyncFingerprint {
        *self.0.finalize().as_bytes()
    }
}

pub(crate) fn sync_fingerprint_changed(
    cache: &OnceLock<Mutex<Option<SyncFingerprint>>>,
    fingerprint: SyncFingerprint,
) -> bool {
    let cache = cache.get_or_init(|| Mutex::new(None));
    let mut previous = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if previous.as_ref() == Some(&fingerprint) {
        false
    } else {
        *previous = Some(fingerprint);
        true
    }
}

pub(crate) fn clear_sync_fingerprint(cache: &OnceLock<Mutex<Option<SyncFingerprint>>>) {
    if let Some(cache) = cache.get() {
        *cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

pub(crate) fn profile_addons_fingerprint(addons: &[Descriptor]) -> SyncFingerprint {
    let mut fingerprint = Fingerprint::new();
    for addon in addons {
        fingerprint.str(addon.transport_url.as_str());
        fingerprint.str(&addon.manifest.id);
        fingerprint.str(&addon.manifest.name);
        fingerprint.optional_str(addon.manifest.description.as_deref());
        fingerprint.optional_str(addon.manifest.logo.as_ref().map(url::Url::as_str));
        fingerprint.u64(addon.manifest.version.major);
        fingerprint.u64(addon.manifest.version.minor);
        fingerprint.u64(addon.manifest.version.patch);
        fingerprint.str(addon.manifest.version.pre.as_str());
        fingerprint.str(addon.manifest.version.build.as_str());
        for addon_type in &addon.manifest.types {
            fingerprint.str(addon_type);
        }
        fingerprint.bool(addon.manifest.behavior_hints.configurable);
        fingerprint.bool(addon.manifest.behavior_hints.configuration_required);
        for catalog in &addon.manifest.catalogs {
            fingerprint.str(&catalog.id);
            fingerprint.str(&catalog.r#type);
            fingerprint.optional_str(catalog.name.as_deref());
        }
    }
    fingerprint.finish()
}

pub(crate) fn profile_catalogs_fingerprint(addons: &[Descriptor]) -> SyncFingerprint {
    let mut fingerprint = Fingerprint::new();
    for addon in addons {
        fingerprint.str(addon.transport_url.as_str());
        fingerprint.str(&addon.manifest.id);
        for catalog in &addon.manifest.catalogs {
            fingerprint.str(&catalog.id);
            fingerprint.str(&catalog.r#type);
            fingerprint.optional_str(catalog.name.as_deref());
        }
    }
    fingerprint.finish()
}

pub(crate) fn catalog_name_index(addons: &[Descriptor]) -> HashMap<(&str, &str, &str), &str> {
    let entry_count = addons
        .iter()
        .map(|addon| addon.manifest.catalogs.len())
        .sum();
    let mut names = HashMap::with_capacity(entry_count);
    for addon in addons {
        for catalog in &addon.manifest.catalogs {
            names.insert(
                (
                    addon.transport_url.as_str(),
                    catalog.id.as_str(),
                    catalog.r#type.as_str(),
                ),
                catalog.name.as_deref().unwrap_or(catalog.id.as_str()),
            );
        }
    }
    names
}

/// Turns visible virtual-list rows into one coalesced, bounded Core range load.
/// The Core range uses an inclusive `end`, despite being represented by `Range`.
pub(crate) fn spawn_catalog_visibility_loader(
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    scope: CatalogScope,
) -> tokio::sync::mpsc::Sender<usize> {
    let (sender, mut receiver) =
        tokio::sync::mpsc::channel::<usize>(CATALOG_VISIBILITY_QUEUE_CAPACITY);
    let runtime = Arc::clone(runtime);
    tokio::spawn(async move {
        while let Some(first_index) = receiver.recv().await {
            // Let delegates created in the same Slint layout pass enqueue
            // their indices before draining, producing one Core range action.
            tokio::task::yield_now().await;
            let mut first_visible = first_index;
            let mut last_visible = first_index;
            while let Ok(index) = receiver.try_recv() {
                first_visible = first_visible.min(index);
                last_visible = last_visible.max(index);
            }

            let range = runtime.model().ok().and_then(|model| {
                visible_catalog_load_range(scope.catalogs(&model), first_visible, last_visible)
            });
            let Some(range) = range else {
                continue;
            };

            tracing::debug!(?scope, ?range, "loading visible addon catalog rows");
            runtime.dispatch(RuntimeAction {
                field: Some(scope.field()),
                action: Action::CatalogsWithExtra(ActionCatalogsWithExtra::LoadRange(range)),
            });
        }
    });
    sender
}

pub(crate) fn queue_visible_catalog(sender: &tokio::sync::mpsc::Sender<usize>, index: i32) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    // A full queue already contains more visible rows than a viewport can
    // display. Dropping another index keeps the UI callback non-blocking; the
    // worker coalesces the queued indices into a preload window.
    let _ = sender.try_send(index);
}

fn visible_catalog_load_range(
    catalogs: &CatalogsWithExtra,
    first_visible: usize,
    last_visible: usize,
) -> Option<Range<usize>> {
    catalogs.selected.as_ref()?;
    bounded_catalog_load_range(
        catalogs.catalogs.len(),
        first_visible,
        last_visible,
        |index| {
            catalogs.catalogs[index]
                .first()
                .is_none_or(|page| page.content.is_none())
        },
    )
}

fn bounded_catalog_load_range(
    catalog_count: usize,
    first_visible: usize,
    last_visible: usize,
    mut is_unloaded: impl FnMut(usize) -> bool,
) -> Option<Range<usize>> {
    if catalog_count == 0 || first_visible >= catalog_count {
        return None;
    }
    let last_visible = last_visible.max(first_visible).min(catalog_count - 1);
    let start = first_visible.saturating_sub(CATALOG_PRELOAD_ROWS);
    let end = last_visible
        .saturating_add(CATALOG_PRELOAD_ROWS)
        .min(catalog_count - 1);
    (start..=end)
        .any(&mut is_unloaded)
        .then_some(start..end.saturating_add(1))
}

pub(crate) fn fingerprint_catalog_projection(
    fingerprint: &mut Fingerprint,
    catalogs: &CatalogsWithExtra,
) {
    fingerprint.usize(catalogs.catalogs.len());
    for catalog in &catalogs.catalogs {
        fingerprint.usize(catalog.len());
        let mut remaining_items = CATALOG_PREVIEW_SIZE;
        for page in catalog {
            fingerprint.str(page.request.base.as_str());
            fingerprint.str(&page.request.path.r#type);
            fingerprint.str(&page.request.path.id);
            for extra in &page.request.path.extra {
                fingerprint.str(&extra.name);
                fingerprint.str(&extra.value);
            }
            match &page.content {
                None => fingerprint.u64(0),
                Some(Loadable::Loading) => fingerprint.u64(1),
                Some(Loadable::Ready(items)) => {
                    fingerprint.u64(2);
                    for item in items.iter().take(remaining_items) {
                        fingerprint.str(&item.id);
                        fingerprint.str(&item.r#type);
                        fingerprint.str(&item.name);
                        fingerprint.optional_str(item.poster.as_ref().map(url::Url::as_str));
                        fingerprint.optional_str(item.release_info.as_deref());
                        fingerprint.optional_str(item.behavior_hints.default_video_id.as_deref());
                    }
                    remaining_items =
                        remaining_items.saturating_sub(items.len().min(remaining_items));
                }
                Some(Loadable::Err(error)) => {
                    fingerprint.u64(3);
                    fingerprint.str(&error.to_string());
                }
            }
        }
    }
}

/// Projects exactly one lightweight Slint row for one Core catalog. Unloaded
/// catalogs remain in the model as placeholders so ListView virtualization can
/// request them as they approach the viewport.
pub(crate) fn project_catalog_section(
    catalog_index: usize,
    catalog: &Catalog<MetaItemPreview>,
    catalog_name: &str,
) -> Option<BoardSection> {
    let first_page = catalog.first()?;
    let request = &first_page.request;
    let (loading, error_message, project_items) = match &first_page.content {
        None | Some(Loadable::Loading) => (true, String::new(), false),
        Some(Loadable::Ready(_)) => (false, String::new(), true),
        Some(Loadable::Err(ResourceError::EmptyContent)) => return None,
        Some(Loadable::Err(error)) => (false, error.to_string(), false),
    };

    let mut cards = Vec::with_capacity(CATALOG_PREVIEW_SIZE);
    if project_items {
        for item in catalog
            .iter()
            .filter_map(|page| page.content.as_ref().and_then(Loadable::ready))
            .flatten()
            .take(CATALOG_PREVIEW_SIZE)
        {
            cards.push(catalog_media_card(item));
        }
    }

    let mut media_type = request.path.r#type.clone();
    if let Some(first) = media_type.get_mut(0..1) {
        first.make_ascii_uppercase();
    }

    let poster_shape = catalog
        .iter()
        .filter_map(|page| page.content.as_ref().and_then(Loadable::ready))
        .flatten()
        .next()
        .map(|item| match item.poster_shape {
            stremio_core::types::resource::PosterShape::Landscape => "landscape",
            stremio_core::types::resource::PosterShape::Square => "square",
            _ => "poster",
        })
        .unwrap_or("poster");

    Some(BoardSection {
        title: format!("{catalog_name} – {media_type}").into(),
        r_type: request.path.r#type.as_str().into(),
        catalog_id: request.path.id.as_str().into(),
        addon_base: request.base.as_str().into(),
        catalog_index: i32::try_from(catalog_index).unwrap_or(i32::MAX),
        loading,
        error_message: error_message.into(),
        items: ModelRc::new(slint::VecModel::from(cards)),
        is_continue_watching: false,
        poster_shape: poster_shape.into(),
    })
}

pub(crate) fn catalog_media_card(item: &MetaItemPreview) -> MediaCardItem {
    MediaCardItem {
        id: item.id.as_str().into(),
        media_type: item.r#type.as_str().into(),
        video_id: item
            .behavior_hints
            .default_video_id
            .as_deref()
            .unwrap_or_default()
            .into(),
        title: item.name.as_str().into(),
        poster_url: item
            .poster
            .as_ref()
            .map(url::Url::as_str)
            .unwrap_or_default()
            .into(),
        poster: crate::image_cache::get_cached_image(&item.poster),
        description: item.release_info.as_deref().unwrap_or_default().into(),
        show_checkmark: false,
        show_progress: false,
        progress_value: 0.0,
        new_videos: 0,
        can_play: false,
    }
}

/// Reproduce `LibraryItemDeepLinks` plus the web client's
/// `detailsVideosFirst` choice without allocating deep-link strings.
pub(crate) fn library_details_video_id<'a>(
    state_video_id: Option<&'a str>,
    time_offset: u64,
    default_video_id: Option<&'a str>,
    videos_first: bool,
) -> Option<&'a str> {
    if videos_first {
        // A regular Library card prefers the metadata/videos route. Core only
        // suppresses that route when the item supplies a default video.
        default_video_id
    } else {
        // Continue Watching prefers the progressed video and then Core's
        // default video before falling back to metadata-only details.
        state_video_id
            .filter(|_| time_offset > 0)
            .or(default_video_id)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MediaGridMetrics {
    pub columns: usize,
}

/// Keep Rust-side row virtualization aligned with the responsive Slint media grid.
pub(crate) fn media_grid_metrics(ui: &MainWindow) -> MediaGridMetrics {
    let window = ui.window();
    let logical_width = window.size().width as f32 / window.scale_factor().max(1.0);
    let provisional_unit = if logical_width > 2800.0 {
        18.0
    } else if logical_width > 2200.0 {
        16.0
    } else if logical_width > 1600.0 {
        15.0
    } else {
        14.0
    };
    let provisional_page_width = logical_width - 6.0 * provisional_unit;
    let unit = if provisional_page_width > 2700.0 {
        18.0
    } else if provisional_page_width > 2100.0 {
        16.0
    } else if provisional_page_width > 1500.0 {
        15.0
    } else {
        14.0
    };
    let page_width = (logical_width - 6.0 * unit).max(1.0);
    let columns: usize = if page_width > 2100.0 {
        9
    } else if page_width > 1500.0 {
        8
    } else if page_width > 1200.0 {
        7
    } else if page_width > 900.0 {
        6
    } else {
        4
    };
    MediaGridMetrics { columns }
}

// Helper to chunk an owned vector into rows of specified size without cloning
pub fn chunk_vector_owned<T>(items: Vec<T>, chunk_size: usize) -> Vec<Vec<T>> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::with_capacity(items.len().div_ceil(chunk_size));
    let mut iter = items.into_iter();
    loop {
        let chunk: Vec<T> = iter.by_ref().take(chunk_size).collect();
        if chunk.is_empty() {
            break;
        }
        rows.push(chunk);
    }
    rows
}

/// Updates only the cards whose async image requests just completed. Keeping
/// the existing `ModelRc`s alive avoids rebuilding every catalog row when a
/// poster becomes available.
pub(crate) fn refresh_cached_media_images(ui: &MainWindow, urls: &[String]) -> usize {
    let images = urls
        .iter()
        .filter_map(|url| cached_image(url).map(|image| (url.as_str(), image)))
        .collect::<HashMap<_, _>>();
    let mut updated = 0;

    updated += patch_cards(&ui.get_board_continue_watching(), &images);
    let sections = ui.get_board_sections();
    for index in 0..sections.row_count() {
        if let Some(section) = sections.row_data(index) {
            updated += patch_cards(&section.items, &images);
        }
    }

    let discover = ui.get_discover_rows();
    for index in 0..discover.row_count() {
        if let Some(row) = discover.row_data(index) {
            updated += patch_cards(&row.cols, &images);
        }
    }

    let library = ui.get_library_rows();
    for index in 0..library.row_count() {
        if let Some(row) = library.row_data(index) {
            updated += patch_cards(&row.cols, &images);
        }
    }

    let search = ui.get_search_sections();
    for index in 0..search.row_count() {
        if let Some(section) = search.row_data(index) {
            updated += patch_cards(&section.items, &images);
        }
    }

    updated += patch_search_suggestions(&ui.get_search_suggestions(), &images);
    updated += patch_addons(&ui.get_addons_list(), &images);
    updated += patch_episode_items(&ui.get_detail_episodes(), &images);
    updated += patch_episode_items(&ui.get_player_episodes(), &images);

    let mut addon_details = ui.get_addon_details_addon();
    if let Some(logo) = images.get(addon_details.logo_url.as_str()) {
        addon_details.logo = logo.clone();
        ui.set_addon_details_addon(addon_details);
        updated += 1;
    }

    let detail_poster_url = ui.get_detail_poster_url();
    if let Some(poster) = images.get(detail_poster_url.as_str()) {
        ui.set_detail_poster(poster.clone());
        updated += 1;
    }
    let detail_logo_url = ui.get_detail_logo_url();
    if let Some(logo) = images.get(detail_logo_url.as_str()) {
        ui.set_detail_logo(logo.clone());
        updated += 1;
    }
    let detail_background_url = ui.get_detail_background_url();
    if let Some(background) = images.get(detail_background_url.as_str()) {
        ui.set_detail_background(background.clone());
        updated += 1;
    }
    let discover_preview_poster_url = ui.get_discover_preview_poster_url();
    if let Some(poster) = images.get(discover_preview_poster_url.as_str()) {
        ui.set_discover_preview_poster(poster.clone());
        updated += 1;
    }
    let discover_preview_logo_url = ui.get_discover_preview_logo_url();
    if let Some(logo) = images.get(discover_preview_logo_url.as_str()) {
        ui.set_discover_preview_logo(logo.clone());
        updated += 1;
    }
    let player_series_logo_url = ui.get_player_series_logo_url();
    if let Some(logo) = images.get(player_series_logo_url.as_str()) {
        ui.set_player_series_logo(logo.clone());
        updated += 1;
    }

    let calendar = ui.get_calendar_rows();
    for row_index in 0..calendar.row_count() {
        let Some(row) = calendar.row_data(row_index) else {
            continue;
        };
        for cell_index in 0..row.cells.row_count() {
            let Some(cell) = row.cells.row_data(cell_index) else {
                continue;
            };
            updated += patch_calendar_items(&cell.items, &images);
        }
    }

    updated
}

fn cached_image(url: &str) -> Option<slint::Image> {
    let image = crate::image_cache::get_cached_image_url(url);
    (image.size().width > 0).then_some(image)
}

fn patch_cards(model: &ModelRc<MediaCardItem>, images: &HashMap<&str, slint::Image>) -> usize {
    let mut updated = 0;
    for index in 0..model.row_count() {
        let Some(mut card) = model.row_data(index) else {
            continue;
        };
        let Some(poster) = images.get(card.poster_url.as_str()) else {
            continue;
        };
        card.poster = poster.clone();
        model.set_row_data(index, card);
        updated += 1;
    }
    updated
}

fn patch_search_suggestions(
    model: &ModelRc<SearchSuggestion>,
    images: &HashMap<&str, slint::Image>,
) -> usize {
    let mut updated = 0;
    for index in 0..model.row_count() {
        let Some(mut suggestion) = model.row_data(index) else {
            continue;
        };
        let Some(poster) = images.get(suggestion.poster_url.as_str()) else {
            continue;
        };
        suggestion.poster = poster.clone();
        model.set_row_data(index, suggestion);
        updated += 1;
    }
    updated
}

fn patch_addons(model: &ModelRc<AddonItem>, images: &HashMap<&str, slint::Image>) -> usize {
    let mut updated = 0;
    for index in 0..model.row_count() {
        let Some(mut addon) = model.row_data(index) else {
            continue;
        };
        let Some(logo) = images.get(addon.logo_url.as_str()) else {
            continue;
        };
        addon.logo = logo.clone();
        model.set_row_data(index, addon);
        updated += 1;
    }
    updated
}

fn patch_calendar_items(
    model: &ModelRc<CalendarMediaItem>,
    images: &HashMap<&str, slint::Image>,
) -> usize {
    let mut updated = 0;
    for index in 0..model.row_count() {
        let Some(mut item) = model.row_data(index) else {
            continue;
        };
        let Some(poster) = images.get(item.poster_url.as_str()) else {
            continue;
        };
        item.poster = poster.clone();
        model.set_row_data(index, item);
        updated += 1;
    }
    updated
}

fn patch_episode_items(
    model: &ModelRc<EpisodeItem>,
    images: &HashMap<&str, slint::Image>,
) -> usize {
    let mut updated = 0;
    for index in 0..model.row_count() {
        let Some(mut episode) = model.row_data(index) else {
            continue;
        };
        let Some(thumbnail) = images.get(episode.thumbnail_url.as_str()) else {
            continue;
        };
        episode.thumbnail = thumbnail.clone();
        model.set_row_data(index, episode);
        updated += 1;
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_catalog_load_range_preloads_around_visible_rows() {
        let unloaded = [
            false, false, false, false, false, false, false, true, false, false,
        ];

        let range = bounded_catalog_load_range(unloaded.len(), 4, 4, |index| unloaded[index]);

        assert_eq!(range, Some(0..10));
    }

    #[test]
    fn bounded_catalog_load_range_skips_an_already_loaded_window() {
        let range = bounded_catalog_load_range(50, 20, 24, |_| false);

        assert_eq!(range, None);
    }

    #[test]
    fn catalog_media_card_preserves_addon_item_details_identity() {
        let item: MetaItemPreview = serde_json::from_value(serde_json::json!({
            "id": "addon-series-id",
            "type": "series",
            "name": "Addon series",
            "behaviorHints": {
                "defaultVideoId": "addon-episode-id"
            }
        }))
        .expect("valid metadata preview");

        let card = catalog_media_card(&item);

        assert_eq!(
            (card.media_type.to_string(), card.video_id.to_string()),
            ("series".to_owned(), "addon-episode-id".to_owned())
        );
    }

    #[test]
    fn regular_library_navigation_prefers_metadata_unless_core_has_a_default_video() {
        assert_eq!(
            library_details_video_id(Some("resume-video"), 5_000, None, true),
            None
        );
        assert_eq!(
            library_details_video_id(Some("resume-video"), 5_000, Some("default-video"), true,),
            Some("default-video")
        );
    }

    #[test]
    fn continue_watching_navigation_prefers_progress_then_default_video() {
        assert_eq!(
            library_details_video_id(Some("resume-video"), 5_000, Some("default-video"), false,),
            Some("resume-video")
        );
        assert_eq!(
            library_details_video_id(Some("stale-video"), 0, Some("default-video"), false,),
            Some("default-video")
        );
    }
}

use std::{
    collections::HashMap,
    sync::{
        RwLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

use stremio_core::{
    models::{common::Loadable, meta_details::MetaDetails, player::Selected},
    types::{
        addon::{Descriptor, ResourcePath},
        profile::Settings,
        resource::{MetaItemPreview, Stream},
    },
};

use crate::app_model::AppModel;

/// Everything the hover trailer popup needs about one catalog item.
#[derive(Clone, Debug)]
pub struct TrailerPreview {
    pub meta_id: String,
    pub title: String,
    pub year: String,
    pub runtime: String,
    pub rating: String,
    pub description: String,
    pub genres: Vec<String>,
    pub poster: Option<url::Url>,
    pub in_library: bool,
    /// `None` when the item exposes no playable trailer.
    pub stream_url: Option<String>,
    /// Retained so "Add to Library" does not need a second metadata lookup.
    pub preview: Option<MetaItemPreview>,
}

/// Genre chips shown in the popup, capped to keep the card one line tall.
const TRAILER_PREVIEW_GENRE_LIMIT: usize = 3;

/// Presentation data for a stream whose full core selection remains in Rust.
#[derive(Clone, Debug)]
pub struct StreamSelectionView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub provider: String,
    pub thumbnail: Option<String>,
    pub progress: f32,
    pub score: i32,
    pub score_reasons: Vec<String>,
    pub filtered: bool,
}

#[derive(Clone)]
struct RegisteredSelection {
    selected: Selected,
    stream_name: String,
}

fn stream_selection_id(resource_index: usize, stream_index: usize) -> String {
    format!("stream:{resource_index}:{stream_index}")
}

/// Keeps full core stream selections out of the Slint presentation model.
#[derive(Default)]
pub struct PlaybackSelections {
    entries: RwLock<HashMap<String, RegisteredSelection>>,
    trailer_id: RwLock<Option<String>>,
    ranking_mode: AtomicU8,
    show_filtered: AtomicBool,
    source_generation: AtomicU64,
    source_key: RwLock<String>,
    debrid_availability: RwLock<HashMap<String, stream_ranking::DebridAvailability>>,
}

impl PlaybackSelections {
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        *self
            .trailer_id
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.debrid_availability
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.source_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Atomically replaces visible stream selections and returns their UI views.
    pub fn rebuild(
        &self,
        details: &MetaDetails,
        addons: &[Descriptor],
    ) -> Vec<StreamSelectionView> {
        let meta_request = details
            .selected
            .as_ref()
            .and_then(|selected| {
                details
                    .meta_items
                    .iter()
                    .find(|resource| resource.request.path.eq_no_extra(&selected.meta_path))
            })
            .map(|resource| resource.request.clone());
        let source_key = meta_request
            .as_ref()
            .map(|request| format!("{}:{}", request.path.r#type, request.path.id))
            .unwrap_or_default();
        let source_changed = {
            let mut current = self
                .source_key
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *current == source_key {
                false
            } else {
                *current = source_key;
                true
            }
        };
        if source_changed {
            self.source_generation.fetch_add(1, Ordering::AcqRel);
            self.debrid_availability
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }

        let mut next_entries = HashMap::new();
        let mut views = Vec::new();
        let mut rank_inputs = Vec::new();
        let provider_names: HashMap<&str, &str> = addons
            .iter()
            .map(|addon| (addon.transport_url.as_str(), addon.manifest.name.as_str()))
            .collect();

        let trailer_id = details
            .meta_items
            .iter()
            .find_map(|resource| {
                let Loadable::Ready(meta) = resource.content.as_ref()? else {
                    return None;
                };
                meta.preview.trailer_streams.first().cloned()
            })
            .map(|stream| {
                let id = "trailer".to_owned();
                next_entries.insert(
                    id.clone(),
                    RegisteredSelection {
                        selected: Selected {
                            stream,
                            stream_request: None,
                            meta_request: meta_request.clone(),
                            subtitles_path: None,
                        },
                        stream_name: "Trailer".to_owned(),
                    },
                );
                id
            });

        for (resource_index, resource) in details.streams.iter().enumerate() {
            let Some(Loadable::Ready(streams)) = &resource.content else {
                continue;
            };

            for (stream_index, stream) in streams.iter().enumerate() {
                // Stable IDs let the event loop skip replacing an unchanged
                // Slint stream model when an unrelated core field updates.
                let id = stream_selection_id(resource_index, stream_index);
                let name = stream.name.clone().unwrap_or_else(|| "Stream".to_owned());
                let description = stream.description.clone().unwrap_or_default();
                let subtitles_path = ResourcePath {
                    resource: "subtitles".to_owned(),
                    r#type: resource.request.path.r#type.clone(),
                    id: resource.request.path.id.clone(),
                    extra: Vec::new(),
                };
                let selected = Selected {
                    stream: stream.clone(),
                    stream_request: Some(resource.request.clone()),
                    meta_request: meta_request.clone(),
                    subtitles_path: Some(subtitles_path),
                };

                let seeders = stream
                    .behavior_hints
                    .other
                    .get("seeders")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|seeders| u32::try_from(seeders).ok())
                    .or_else(|| stream_ranking::parse_seeders_from_text(&description))
                    .or_else(|| stream_ranking::parse_seeders_from_text(&name));
                let size_bytes = stream
                    .behavior_hints
                    .video_size
                    .or_else(|| stream_ranking::parse_size_bytes_from_text(&description))
                    .or_else(|| stream_ranking::parse_size_bytes_from_text(&name));
                let formatted_description = stream_ranking::format_stream_description(
                    &name,
                    &description,
                    size_bytes,
                    seeders,
                );

                next_entries.insert(
                    id.clone(),
                    RegisteredSelection {
                        selected,
                        stream_name: if formatted_description.is_empty() {
                            name.clone()
                        } else {
                            formatted_description.clone()
                        },
                    },
                );
                views.push(StreamSelectionView {
                    id: id.clone(),
                    name: name.clone(),
                    description: formatted_description.clone(),
                    thumbnail: stream.thumbnail.clone(),
                    progress: stream
                        .behavior_hints
                        .other
                        .get("progress")
                        .and_then(serde_json::Value::as_f64)
                        .map(|progress| (progress / 100.0).clamp(0.0, 1.0) as f32)
                        .unwrap_or_default(),
                    provider: provider_names
                        .get(resource.request.base.as_str())
                        .map(|name| (*name).to_owned())
                        .unwrap_or_else(|| {
                            resource
                                .request
                                .base
                                .host_str()
                                .unwrap_or("Addon")
                                .to_owned()
                        }),
                    score: 0,
                    score_reasons: Vec::new(),
                    filtered: false,
                });
                rank_inputs.push(stream_ranking::RankInput {
                    id,
                    name: name.clone(),
                    description: formatted_description,
                    addon: resource.request.base.to_string(),
                    original_index: rank_inputs.len(),
                    size_bytes,
                    seeders,
                    debrid: self
                        .debrid_availability
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&stream_selection_id(resource_index, stream_index))
                        .copied(),
                });
            }
        }

        let ranked = stream_ranking::rank_streams(
            rank_inputs,
            self.ranking_mode(),
            self.show_filtered.load(Ordering::Acquire),
        );
        let ranking = ranked
            .into_iter()
            .enumerate()
            .map(|(position, ranked)| {
                let reasons = ranked
                    .reasons
                    .into_iter()
                    .map(|reason| {
                        let prefix = if reason.points > 0 { "+" } else { "" };
                        format!("{prefix}{} {}", reason.points, reason.label)
                    })
                    .collect::<Vec<_>>();
                (
                    ranked.input.id,
                    (position, ranked.score, reasons, ranked.filtered),
                )
            })
            .collect::<HashMap<_, _>>();
        views.retain(|view| ranking.contains_key(&view.id));
        for view in &mut views {
            if let Some((_, score, reasons, filtered)) = ranking.get(&view.id) {
                view.score = *score;
                view.score_reasons.clone_from(reasons);
                view.filtered = *filtered;
            }
        }
        views.sort_by_key(|view| {
            ranking
                .get(&view.id)
                .map(|rank| rank.0)
                .unwrap_or(usize::MAX)
        });

        match self.entries.write() {
            Ok(mut entries) => *entries = next_entries,
            Err(poisoned) => *poisoned.into_inner() = next_entries,
        }
        match self.trailer_id.write() {
            Ok(mut current) => *current = trailer_id,
            Err(poisoned) => *poisoned.into_inner() = trailer_id,
        }
        views
    }

    pub fn trailer_id(&self) -> Option<String> {
        match self.trailer_id.read() {
            Ok(id) => id.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn set_ranking_mode(&self, mode: stream_ranking::RankingMode) {
        self.ranking_mode
            .store(ranking_mode_byte(mode), Ordering::Release);
    }

    pub fn ranking_mode(&self) -> stream_ranking::RankingMode {
        match self.ranking_mode.load(Ordering::Acquire) {
            1 => stream_ranking::RankingMode::Quality,
            2 => stream_ranking::RankingMode::Smallest,
            3 => stream_ranking::RankingMode::Seeders,
            4 => stream_ranking::RankingMode::Original,
            _ => stream_ranking::RankingMode::Smart,
        }
    }

    pub fn set_show_filtered(&self, show: bool) {
        self.show_filtered.store(show, Ordering::Release);
    }

    pub fn debrid_candidates(&self) -> (u64, Vec<(String, String)>) {
        let availability = self
            .debrid_availability
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entries = self
            .entries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidates = entries
            .iter()
            .filter(|(id, _)| !availability.contains_key(*id))
            .filter_map(|(id, entry)| match &entry.selected.stream.source {
                stremio_core::types::resource::StreamSource::Torrent { info_hash, .. } => {
                    Some((id.clone(), hex::encode(info_hash)))
                }
                _ => None,
            })
            .collect();
        (self.source_generation.load(Ordering::Acquire), candidates)
    }

    pub fn apply_debrid_availability(
        &self,
        generation: u64,
        availability: HashMap<String, stream_ranking::DebridAvailability>,
    ) -> bool {
        if self.source_generation.load(Ordering::Acquire) != generation {
            return false;
        }
        self.debrid_availability
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(availability);
        true
    }

    /// Resolves the hover trailer preview for an arbitrary catalog item.
    ///
    /// Hovering happens far away from the details route, so this reads the
    /// metadata previews the loaded catalogs already carry instead of
    /// dispatching a metadata request that would disturb the current route.
    /// Items reachable only through the library still project their card
    /// metadata; they simply have no trailer to play.
    pub fn resolve_trailer_for_meta_id(
        &self,
        model: &AppModel,
        meta_id: &str,
    ) -> Option<TrailerPreview> {
        let in_library = model
            .ctx
            .library
            .items
            .get(meta_id)
            .is_some_and(|item| !item.removed);
        let streaming_server_url = model.streaming_server.base_url.as_ref();
        let settings = &model.ctx.profile.settings;

        if let Some(preview) = find_meta_preview(model, meta_id) {
            let stream_url = preview
                .trailer_streams
                .first()
                .and_then(|stream| trailer_stream_url(stream, streaming_server_url, settings));
            return Some(TrailerPreview {
                meta_id: preview.id.clone(),
                title: preview.name.clone(),
                year: preview_year(preview),
                runtime: preview.runtime.clone().unwrap_or_default(),
                rating: preview
                    .links
                    .iter()
                    .find(|link| link.category.eq_ignore_ascii_case("imdb"))
                    .map(|link| link.name.clone())
                    .unwrap_or_default(),
                description: preview.description.clone().unwrap_or_default(),
                genres: preview
                    .links
                    .iter()
                    .filter(|link| {
                        link.category.eq_ignore_ascii_case("genre")
                            || link.category.eq_ignore_ascii_case("genres")
                    })
                    .map(|link| link.name.clone())
                    .take(TRAILER_PREVIEW_GENRE_LIMIT)
                    .collect(),
                poster: preview.poster.clone(),
                in_library,
                stream_url,
                preview: Some(preview.clone()),
            });
        }

        let item = model.ctx.library.items.get(meta_id)?;
        Some(TrailerPreview {
            meta_id: item.id.clone(),
            title: item.name.clone(),
            year: String::new(),
            runtime: String::new(),
            rating: String::new(),
            description: String::new(),
            genres: Vec::new(),
            poster: item.poster.clone(),
            in_library,
            stream_url: None,
            preview: None,
        })
    }

    /// Resolves an opaque UI ID back to the full core selection and label.
    pub fn resolve(&self, id: &str) -> Option<(Selected, String)> {
        let entries = match self.entries.read() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        entries
            .get(id)
            .map(|entry| (entry.selected.clone(), entry.stream_name.clone()))
    }
}

/// Searches every loaded catalog for the metadata preview of one item.
fn find_meta_preview<'a>(model: &'a AppModel, meta_id: &str) -> Option<&'a MetaItemPreview> {
    let details =
        model
            .meta_details
            .meta_items
            .iter()
            .find_map(|resource| match resource.content.as_ref() {
                Some(Loadable::Ready(meta)) if meta.preview.id == meta_id => Some(&meta.preview),
                _ => None,
            });
    if details.is_some() {
        return details;
    }

    model
        .board
        .catalogs
        .iter()
        .chain(model.search.catalogs.iter())
        .flatten()
        .chain(model.discover.catalog.iter())
        .find_map(|page| match page.content.as_ref() {
            Some(Loadable::Ready(items)) => items.iter().find(|item| item.id == meta_id),
            _ => None,
        })
}

/// Renders the trailer stream as a URL the preview MPV worker can open.
fn trailer_stream_url(
    stream: &Stream,
    streaming_server_url: Option<&url::Url>,
    settings: &Settings,
) -> Option<String> {
    if let stremio_core::types::resource::StreamSource::YouTube { yt_id } = &stream.source {
        return Some(format!("https://www.youtube.com/watch?v={yt_id}"));
    }
    let links =
        stremio_core::deep_links::StreamDeepLinks::from((stream, streaming_server_url, settings));
    links.external_player.streaming
}

fn preview_year(preview: &MetaItemPreview) -> String {
    preview
        .release_info
        .clone()
        .or_else(|| preview.released.map(|date| date.format("%Y").to_string()))
        .unwrap_or_default()
}

fn ranking_mode_byte(mode: stream_ranking::RankingMode) -> u8 {
    match mode {
        stream_ranking::RankingMode::Smart => 0,
        stream_ranking::RankingMode::Quality => 1,
        stream_ranking::RankingMode::Smallest => 2,
        stream_ranking::RankingMode::Seeders => 3,
        stream_ranking::RankingMode::Original => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::stream_selection_id;

    #[test]
    fn stream_selection_ids_are_stable_and_resource_scoped() {
        assert_eq!(stream_selection_id(2, 7), "stream:2:7");
        assert_ne!(stream_selection_id(1, 0), stream_selection_id(0, 1));
    }
}

use std::{collections::HashMap, sync::RwLock};

use stremio_core::{
    models::{common::Loadable, meta_details::MetaDetails, player::Selected},
    types::addon::{Descriptor, ResourcePath},
};

/// Presentation data for a stream whose full core selection remains in Rust.
#[derive(Clone, Debug)]
pub struct StreamSelectionView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub provider: String,
    pub thumbnail: Option<String>,
    pub progress: f32,
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
}

impl PlaybackSelections {
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

        let mut next_entries = HashMap::new();
        let mut views = Vec::new();
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

                next_entries.insert(
                    id.clone(),
                    RegisteredSelection {
                        selected,
                        stream_name: if description.is_empty() {
                            name.clone()
                        } else {
                            description.clone()
                        },
                    },
                );
                views.push(StreamSelectionView {
                    id,
                    name,
                    description,
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
                });
            }
        }

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

#[cfg(test)]
mod tests {
    use super::stream_selection_id;

    #[test]
    fn stream_selection_ids_are_stable_and_resource_scoped() {
        assert_eq!(stream_selection_id(2, 7), "stream:2:7");
        assert_ne!(stream_selection_id(1, 0), stream_selection_id(0, 1));
    }
}

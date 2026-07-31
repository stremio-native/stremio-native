use std::sync::{Mutex, OnceLock};

use futures::{StreamExt, stream::FuturesUnordered};
use media_integrations::ProviderKind;

static LAST_REQUEST: OnceLock<Mutex<Option<(String, String)>>> = OnceLock::new();

pub fn request(ui_weak: slint::Weak<crate::MainWindow>, external_id: String) {
    tokio::spawn(async move {
        let Ok(profile_id) = crate::profiles::active_profile_id().await else {
            return;
        };
        let request_key = (profile_id.as_str().to_owned(), external_id.clone());
        {
            let mut last = LAST_REQUEST
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.as_ref() == Some(&request_key) {
                return;
            }
            *last = Some(request_key);
        }
        let id_for_ui = external_id.clone();
        let weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_detail_enrichment_id(id_for_ui.into());
                ui.set_detail_enrichment_summary("".into());
                ui.set_detail_enrichment_attribution_url("".into());
            }
        });
        let Ok(providers) =
            crate::integrations::enabled_metadata_providers(profile_id.as_str()).await
        else {
            return;
        };
        let region = crate::config::load_config().region;
        let mut pending = FuturesUnordered::new();
        for provider in providers.into_iter().filter(|provider| {
            if external_id.starts_with("tt") {
                !matches!(provider.kind(), ProviderKind::Kitsu | ProviderKind::AniZip)
            } else {
                true
            }
        }) {
            let id = external_id.clone();
            let region = region.clone();
            pending.push(async move {
                let kind = provider.kind();
                let enriched = provider.enrich(&id).await;
                let watch = match &enriched {
                    Ok(meta) if kind == ProviderKind::Tmdb => {
                        let watch_id = meta
                            .external_ids
                            .get("tmdb")
                            .map(String::as_str)
                            .unwrap_or(id.as_str());
                        provider
                            .watch_providers(watch_id, &region)
                            .await
                            .unwrap_or_default()
                    }
                    _ => Vec::new(),
                };
                (kind, enriched, watch)
            });
        }
        let mut parts = Vec::<String>::new();
        let mut attribution_url = String::new();
        let mut watch_names = Vec::<String>::new();
        while let Some((kind, enriched, watch)) = pending.next().await {
            let Ok(enriched) = enriched else {
                tracing::debug!(
                    provider = kind.display_name(),
                    "optional metadata provider unavailable"
                );
                continue;
            };
            let mut part = kind.display_name().to_owned();
            if let Some(rating) = enriched.rating {
                part.push_str(&format!(" ★ {rating:.1}"));
            }
            if let Some(attribution) = enriched.attribution {
                if attribution_url.is_empty() {
                    attribution_url = attribution.url;
                }
                part.push_str(&format!(" · {}", attribution.label));
            }
            parts.push(part);
            for offer in watch {
                if !watch_names.contains(&offer.provider_name) {
                    watch_names.push(offer.provider_name);
                }
            }
            let mut summary = parts.join("  |  ");
            if !watch_names.is_empty() {
                summary.push_str(&format!(
                    "  |  Where to watch ({}): {}",
                    region,
                    watch_names.join(", ")
                ));
            }
            project_if_current(
                ui_weak.clone(),
                external_id.clone(),
                summary,
                attribution_url.clone(),
            );
        }
    });
}

pub fn clear() {
    *LAST_REQUEST
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn project_if_current(
    ui_weak: slint::Weak<crate::MainWindow>,
    external_id: String,
    summary: String,
    attribution_url: String,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade()
            && ui.get_detail_enrichment_id().as_str() == external_id
        {
            ui.set_detail_enrichment_summary(summary.into());
            ui.set_detail_enrichment_attribution_url(attribution_url.into());
        }
    });
}

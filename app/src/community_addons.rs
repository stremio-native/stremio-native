use std::{sync::OnceLock, time::Duration};

use moka::future::Cache;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AddonSort {
    Trending,
    Rating,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommunityAddonFilters {
    pub sort: Option<AddonSort>,
    pub language: Option<String>,
    pub resource_type: Option<String>,
    pub page: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommunityAddon {
    pub name: String,
    pub description: String,
    pub manifest_url: String,
    pub logo: Option<String>,
    pub language: Option<String>,
    pub resource_types: Vec<String>,
    pub rating: Option<f64>,
    pub trending_score: Option<f64>,
    pub adult: bool,
    pub configurable: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommunityAddonPage {
    pub addons: Vec<CommunityAddon>,
    pub has_next_page: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CommunityAddonError {
    #[error("community endpoint URL is invalid")]
    InvalidEndpoint,
    #[error("community endpoint is unavailable")]
    Unavailable,
    #[error("community endpoint returned invalid data")]
    InvalidResponse,
    #[error("addon configuration URL is not trusted")]
    UntrustedConfigurationUrl,
}

#[derive(Clone)]
pub struct CommunityAddonClient {
    endpoint: url::Url,
    client: reqwest::Client,
    cache: Cache<String, CommunityAddonPage>,
}

impl CommunityAddonClient {
    pub fn new(endpoint: &str) -> Result<Self, CommunityAddonError> {
        let endpoint =
            url::Url::parse(endpoint).map_err(|_| CommunityAddonError::InvalidEndpoint)?;
        if endpoint.scheme() != "https" && endpoint.host_str() != Some("localhost") {
            return Err(CommunityAddonError::InvalidEndpoint);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("Stremio-Native/1")
            .build()
            .map_err(|_| CommunityAddonError::Unavailable)?;
        Ok(Self {
            endpoint,
            client,
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(15 * 60))
                .max_capacity(64)
                .build(),
        })
    }

    pub async fn fetch(
        &self,
        filters: &CommunityAddonFilters,
    ) -> Result<CommunityAddonPage, CommunityAddonError> {
        let cache_key =
            serde_json::to_string(filters).map_err(|_| CommunityAddonError::InvalidResponse)?;
        if let Some(cached) = self.cache.get(&cache_key).await {
            return Ok(cached);
        }
        let mut endpoint = self.endpoint.clone();
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("page", &filters.page.to_string());
            if let Some(sort) = filters.sort {
                query.append_pair(
                    "sort",
                    match sort {
                        AddonSort::Trending => "trending",
                        AddonSort::Rating => "rating",
                    },
                );
            }
            if let Some(language) = filters.language.as_deref() {
                query.append_pair("language", language);
            }
            if let Some(resource_type) = filters.resource_type.as_deref() {
                query.append_pair("type", resource_type);
            }
        }
        let response = self
            .client
            .get(endpoint)
            .send()
            .await
            .map_err(|_| CommunityAddonError::Unavailable)?;
        if !response.status().is_success() {
            return Err(CommunityAddonError::Unavailable);
        }
        let page = response
            .json::<CommunityAddonPage>()
            .await
            .map_err(|_| CommunityAddonError::InvalidResponse)?;
        let page = sanitize_page(page);
        self.cache.insert(cache_key, page.clone()).await;
        Ok(page)
    }
}

pub fn visible_for_profile(addon: &CommunityAddon, role: crate::profiles::ProfileRole) -> bool {
    !addon.adult || role == crate::profiles::ProfileRole::Owner
}

pub fn validate_configuration_url(
    manifest_url: &str,
    configuration_url: &str,
) -> Result<url::Url, CommunityAddonError> {
    let manifest = url::Url::parse(manifest_url)
        .map_err(|_| CommunityAddonError::UntrustedConfigurationUrl)?;
    let configuration = url::Url::parse(configuration_url)
        .map_err(|_| CommunityAddonError::UntrustedConfigurationUrl)?;
    if configuration.scheme() != "https"
        || configuration.host_str().is_none()
        || manifest.host_str() != configuration.host_str()
    {
        return Err(CommunityAddonError::UntrustedConfigurationUrl);
    }
    Ok(configuration)
}

fn sanitize_page(mut page: CommunityAddonPage) -> CommunityAddonPage {
    page.addons.retain(|addon| {
        url::Url::parse(&addon.manifest_url)
            .ok()
            .is_some_and(|url| url.scheme() == "https" || url.host_str() == Some("localhost"))
    });
    for addon in &mut page.addons {
        addon.name = addon.name.trim().chars().take(120).collect();
        addon.description = addon.description.trim().chars().take(2_000).collect();
    }
    page
}

pub fn default_client() -> &'static CommunityAddonClient {
    static CLIENT: OnceLock<CommunityAddonClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        CommunityAddonClient::new("https://stremio-addons.net/api/addons")
            .expect("default community addon endpoint must be valid")
    })
}

pub fn setup(ui: &crate::MainWindow) {
    let ui_weak = ui.as_weak();
    ui.on_addons_community_filters_changed({
        let ui_weak = ui_weak.clone();
        move |media_type, sort, language, query| {
            let filters = filters_from_ui(&media_type, &sort, &language);
            let query = query.to_string();
            let unlocked = ui_weak
                .upgrade()
                .is_some_and(|ui| ui.get_addons_community_adult_unlocked());
            tokio::spawn(fetch_and_project(
                ui_weak.clone(),
                filters,
                query,
                unlocked,
                false,
            ));
        }
    });
    ui.on_addons_community_load_next({
        let ui_weak = ui_weak.clone();
        move |media_type, sort, language, query, page| {
            let mut filters = filters_from_ui(&media_type, &sort, &language);
            filters.page = u32::try_from(page).unwrap_or_default();
            let unlocked = ui_weak
                .upgrade()
                .is_some_and(|ui| ui.get_addons_community_adult_unlocked());
            tokio::spawn(fetch_and_project(
                ui_weak.clone(),
                filters,
                query.to_string(),
                unlocked,
                true,
            ));
        }
    });
    let limiter = std::sync::Arc::new(crate::profiles::PinAttemptLimiter::default());
    ui.on_addons_community_unlock_adult({
        let ui_weak = ui_weak.clone();
        move |pin| {
            let limiter = limiter.clone();
            let ui_weak = ui_weak.clone();
            let pin = pin.to_string();
            tokio::spawn(async move {
                match crate::profiles::authorize_owner_pin(&pin, &limiter).await {
                    Ok(_) => {
                        let query = ui_weak
                            .upgrade()
                            .map(|ui| ui.get_addons_search_query().to_string())
                            .unwrap_or_default();
                        let weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_addons_community_adult_unlocked(true);
                                ui.set_addons_community_owner_pin("".into());
                            }
                        });
                        fetch_and_project(
                            ui_weak,
                            CommunityAddonFilters {
                                sort: Some(AddonSort::Trending),
                                ..Default::default()
                            },
                            query,
                            true,
                            false,
                        )
                        .await;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_error_message(message.into());
                            }
                        });
                    }
                }
            });
        }
    });
}

fn filters_from_ui(
    media_type: &slint::SharedString,
    sort: &slint::SharedString,
    language: &slint::SharedString,
) -> CommunityAddonFilters {
    let resource_type = match media_type.as_str() {
        "Movie" => Some("movie"),
        "Series" => Some("series"),
        "Anime" => Some("anime"),
        "TV Channel" => Some("channel"),
        _ => None,
    }
    .map(ToOwned::to_owned);
    let language = match language.as_str() {
        "English" => Some("en"),
        "Portuguese" => Some("pt"),
        "Arabic" => Some("ar"),
        "Spanish" => Some("es"),
        "French" => Some("fr"),
        "German" => Some("de"),
        _ => None,
    }
    .map(ToOwned::to_owned);
    CommunityAddonFilters {
        sort: Some(if sort.as_str() == "Rating" {
            AddonSort::Rating
        } else {
            AddonSort::Trending
        }),
        language,
        resource_type,
        page: 0,
    }
}

async fn fetch_and_project(
    ui_weak: slint::Weak<crate::MainWindow>,
    filters: CommunityAddonFilters,
    query: String,
    adult_unlocked: bool,
    append: bool,
) {
    let result = default_client().fetch(&filters).await;
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match result {
            Ok(page) => {
                let query = query.trim().to_ascii_lowercase();
                let mut items = if append {
                    let model = ui.get_addons_community_list();
                    (0..model.row_count())
                        .filter_map(|index| model.row_data(index))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                items.extend(
                    page.addons
                        .into_iter()
                        .filter(|addon| !addon.adult || adult_unlocked)
                        .filter(|addon| {
                            query.is_empty()
                                || addon.name.to_ascii_lowercase().contains(&query)
                                || addon.description.to_ascii_lowercase().contains(&query)
                        })
                        .map(|addon| project_community_addon(addon, &ui_weak)),
                );
                ui.set_addons_community_list(slint::ModelRc::new(slint::VecModel::from(items)));
                ui.set_addons_community_available(true);
                ui.set_addons_has_next_page(page.has_next_page);
            }
            Err(error) => {
                ui.set_addons_community_available(false);
                ui.set_error_message(format!("Community addons are unavailable: {error}").into());
            }
        }
    });
}

fn project_community_addon(
    addon: CommunityAddon,
    ui_weak: &slint::Weak<crate::MainWindow>,
) -> crate::AddonItem {
    let logo = addon
        .logo
        .as_deref()
        .and_then(|value| url::Url::parse(value).ok());
    let supports = |kind: &str| {
        addon
            .resource_types
            .iter()
            .any(|value| value.eq_ignore_ascii_case(kind))
    };
    let supports_movie = supports("movie");
    let supports_series = supports("series");
    let supports_anime = supports("anime");
    let supports_tv = supports("channel") || supports("tv");
    let types_label = if addon.resource_types.is_empty() {
        "Other".to_owned()
    } else {
        addon.resource_types.join(", ")
    };
    let score = addon
        .rating
        .map(|rating| format!("★ {rating:.1}"))
        .unwrap_or_default();
    crate::AddonItem {
        id: addon.manifest_url.as_str().into(),
        name: addon.name.into(),
        version: score.into(),
        description: addon.description.into(),
        logo_url: addon.logo.unwrap_or_default().into(),
        logo: crate::image_cache::get_poster_image(&logo, ui_weak),
        is_installed: false,
        transport_url: addon.manifest_url.into(),
        types_label: types_label.into(),
        configurable: addon.configurable,
        configuration_required: false,
        supports_movie,
        supports_series,
        supports_anime,
        supports_tv,
        background_url: "".into(),
        background_image: slint::Image::default(),
        adult: addon.adult,
        p2p: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_pages_stay_in_the_system_browser_and_same_origin() {
        assert!(
            validate_configuration_url(
                "https://addon.example/manifest.json",
                "https://addon.example/configure"
            )
            .is_ok()
        );
        assert_eq!(
            validate_configuration_url(
                "https://addon.example/manifest.json",
                "https://evil.example/configure"
            ),
            Err(CommunityAddonError::UntrustedConfigurationUrl)
        );
    }

    #[test]
    fn adult_addons_are_hidden_outside_owner_profiles() {
        let addon = CommunityAddon {
            name: "Adult".to_owned(),
            description: String::new(),
            manifest_url: "https://addon.example/manifest.json".to_owned(),
            logo: None,
            language: None,
            resource_types: Vec::new(),
            rating: None,
            trending_score: None,
            adult: true,
            configurable: false,
        };
        assert!(!visible_for_profile(
            &addon,
            crate::profiles::ProfileRole::Kids
        ));
        assert!(visible_for_profile(
            &addon,
            crate::profiles::ProfileRole::Owner
        ));
    }
}

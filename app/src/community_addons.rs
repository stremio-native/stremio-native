use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use moka::future::Cache;
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model};

const CACHE_CAPACITY_BYTES: u64 = 8 * 1024 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const CACHE_ENTRY_OVERHEAD: usize = 256;
const PAGE_SIZE: u32 = 25;
const RISING_SEARCH_LIMIT: u32 = 100;
const DIRECTORY_URL: &str = "https://stremio-addons.net";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AddonSort {
    Rising,
    Stars,
    Newest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommunityAddonFilters {
    pub sort: AddonSort,
    pub category: Option<String>,
    pub query: Option<String>,
    pub include_nsfw: bool,
    pub page: u32,
}

impl Default for CommunityAddonFilters {
    fn default() -> Self {
        Self {
            sort: AddonSort::Rising,
            category: None,
            query: None,
            include_nsfw: false,
            page: 1,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommunityAddon {
    pub name: String,
    pub description: String,
    pub manifest_url: String,
    pub logo: Option<String>,
    pub background: Option<String>,
    pub version: String,
    pub resource_types: Vec<String>,
    pub categories: Vec<String>,
    pub stars: u64,
    pub recent_stars: Option<u64>,
    pub adult: bool,
    pub p2p: bool,
    pub configurable: bool,
    pub configuration_required: bool,
    pub configuration_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommunityAddonPage {
    pub addons: Vec<CommunityAddon>,
    pub has_next_page: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAddonPage {
    addons: Vec<ApiAddon>,
    #[serde(default)]
    pagination: Option<ApiPagination>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiAddon {
    manifest_url: String,
    manifest: ApiManifest,
    #[serde(default)]
    stars: u64,
    #[serde(default)]
    recent_stars: Option<u64>,
    #[serde(default)]
    categories: Vec<ApiCategory>,
    #[serde(default)]
    configure_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    logo: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    behavior_hints: ApiBehaviorHints,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiBehaviorHints {
    #[serde(default)]
    configurable: bool,
    #[serde(default)]
    configuration_required: bool,
    #[serde(default)]
    adult: bool,
    #[serde(default)]
    p2p: bool,
}

#[derive(Debug, Deserialize)]
struct ApiCategory {
    name: String,
    slug: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiPagination {
    has_next_page: bool,
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
    cache: Cache<String, Arc<CommunityAddonPage>>,
}

impl CommunityAddonClient {
    pub fn new(endpoint: &str) -> Result<Self, CommunityAddonError> {
        let mut endpoint =
            url::Url::parse(endpoint).map_err(|_| CommunityAddonError::InvalidEndpoint)?;
        if endpoint.scheme() != "https" && endpoint.host_str() != Some("localhost") {
            return Err(CommunityAddonError::InvalidEndpoint);
        }
        if !endpoint.path().ends_with('/') {
            let path = format!("{}/", endpoint.path());
            endpoint.set_path(&path);
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
                .time_to_live(CACHE_TTL)
                .max_capacity(CACHE_CAPACITY_BYTES)
                .weigher(|key: &String, page: &Arc<CommunityAddonPage>| {
                    u32::try_from(community_page_weight(key, page)).unwrap_or(u32::MAX)
                })
                .build(),
        })
    }

    pub async fn fetch(
        &self,
        filters: &CommunityAddonFilters,
    ) -> Result<Arc<CommunityAddonPage>, CommunityAddonError> {
        let cache_key =
            serde_json::to_string(filters).map_err(|_| CommunityAddonError::InvalidResponse)?;
        let endpoint = build_request_url(&self.endpoint, filters)?;
        let filters = filters.clone();
        self.cache
            .try_get_with(cache_key, async move {
                let response = self
                    .client
                    .get(endpoint)
                    .send()
                    .await
                    .map_err(|_| CommunityAddonError::Unavailable)?;
                if !response.status().is_success() {
                    return Err(CommunityAddonError::Unavailable);
                }
                if response
                    .content_length()
                    .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
                {
                    return Err(CommunityAddonError::InvalidResponse);
                }
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|_| CommunityAddonError::InvalidResponse)?;
                let page = parse_community_page(&bytes, &filters)?;
                Ok::<Arc<CommunityAddonPage>, CommunityAddonError>(Arc::new(page))
            })
            .await
            .map_err(|error| (*error).clone())
    }
}

fn build_request_url(
    base: &url::Url,
    filters: &CommunityAddonFilters,
) -> Result<url::Url, CommunityAddonError> {
    let rising = filters.sort == AddonSort::Rising;
    let mut endpoint = base
        .join(if rising { "rising" } else { "addons" })
        .map_err(|_| CommunityAddonError::InvalidEndpoint)?;
    {
        let mut query = endpoint.query_pairs_mut();
        if rising {
            let limit = if filters.query.is_some() {
                RISING_SEARCH_LIMIT
            } else {
                PAGE_SIZE
            };
            query.append_pair("limit", &limit.to_string());
        } else {
            query
                .append_pair("page", &filters.page.max(1).to_string())
                .append_pair("limit", &PAGE_SIZE.to_string())
                .append_pair(
                    "sort_by",
                    match filters.sort {
                        AddonSort::Stars => "stars",
                        AddonSort::Newest | AddonSort::Rising => "createdAt",
                    },
                )
                .append_pair("order", "desc");
            if let Some(search) = filters.query.as_deref() {
                query.append_pair("search", search);
            }
        }
        if let Some(category) = filters.category.as_deref() {
            query.append_pair("category", category);
        }
        if !filters.include_nsfw {
            query.append_pair("nsfw", "exclude");
        }
    }
    Ok(endpoint)
}

fn parse_community_page(
    bytes: &[u8],
    filters: &CommunityAddonFilters,
) -> Result<CommunityAddonPage, CommunityAddonError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(CommunityAddonError::InvalidResponse);
    }
    let response: ApiAddonPage =
        serde_json::from_slice(bytes).map_err(|_| CommunityAddonError::InvalidResponse)?;
    let query = filters
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_ascii_lowercase);
    let addons = response
        .addons
        .into_iter()
        .filter_map(normalize_api_addon)
        .filter(|addon| {
            query.as_ref().is_none_or(|query| {
                addon.name.to_ascii_lowercase().contains(query)
                    || addon.description.to_ascii_lowercase().contains(query)
            })
        })
        .collect();
    Ok(CommunityAddonPage {
        addons,
        has_next_page: filters.sort != AddonSort::Rising
            && response
                .pagination
                .is_some_and(|pagination| pagination.has_next_page),
    })
}

fn normalize_api_addon(api: ApiAddon) -> Option<CommunityAddon> {
    let manifest_url = url::Url::parse(&api.manifest_url).ok()?;
    if manifest_url.scheme() != "https" && manifest_url.host_str() != Some("localhost") {
        return None;
    }
    let categories = api
        .categories
        .iter()
        .map(|category| category.name.trim().to_owned())
        .filter(|category| !category.is_empty())
        .collect::<Vec<_>>();
    let adult = api.manifest.behavior_hints.adult
        || api
            .categories
            .iter()
            .any(|category| category.slug == "nsfw");
    let p2p = api.manifest.behavior_hints.p2p
        || api
            .categories
            .iter()
            .any(|category| category.slug == "torrents");
    let configuration_url = api.configure_url.and_then(|configuration_url| {
        validate_configuration_url(manifest_url.as_str(), &configuration_url)
            .ok()
            .map(|url| url.to_string())
    });
    let configurable = api.manifest.behavior_hints.configurable && configuration_url.is_some();

    let name = api.manifest.name.trim().chars().take(120).collect();
    let description = api
        .manifest
        .description
        .trim()
        .chars()
        .take(2_000)
        .collect();
    let version = api.manifest.version.trim().chars().take(80).collect();

    Some(CommunityAddon {
        name,
        description,
        manifest_url: manifest_url.to_string(),
        logo: api.manifest.logo,
        background: api.manifest.background,
        version,
        resource_types: api.manifest.types,
        categories,
        stars: api.stars,
        recent_stars: api.recent_stars,
        adult,
        p2p,
        configurable,
        configuration_required: api.manifest.behavior_hints.configuration_required,
        configuration_url,
    })
}

fn community_page_weight(key: &str, page: &CommunityAddonPage) -> usize {
    key.len()
        .saturating_add(CACHE_ENTRY_OVERHEAD)
        .saturating_add(
            page.addons
                .iter()
                .map(|addon| {
                    addon
                        .name
                        .len()
                        .saturating_add(addon.description.len())
                        .saturating_add(addon.manifest_url.len())
                        .saturating_add(addon.logo.as_ref().map_or(0, String::len))
                        .saturating_add(addon.background.as_ref().map_or(0, String::len))
                        .saturating_add(addon.version.len())
                        .saturating_add(addon.configuration_url.as_ref().map_or(0, String::len))
                        .saturating_add(addon.resource_types.iter().map(String::len).sum::<usize>())
                        .saturating_add(addon.categories.iter().map(String::len).sum::<usize>())
                        .saturating_add(std::mem::size_of::<CommunityAddon>())
                })
                .sum::<usize>(),
        )
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

pub fn default_client() -> &'static CommunityAddonClient {
    static CLIENT: OnceLock<CommunityAddonClient> = OnceLock::new();
    CLIENT.get_or_init(|| {
        CommunityAddonClient::new("https://stremio-addons.net/api/v0/")
            .expect("default community addon endpoint must be valid")
    })
}

pub fn setup(ui: &crate::MainWindow) {
    let ui_weak = ui.as_weak();
    let request_revision = Arc::new(std::sync::atomic::AtomicU64::new(0));
    ui.on_addons_community_filters_changed({
        let ui_weak = ui_weak.clone();
        let request_revision = request_revision.clone();
        move |media_type, sort, category, query| {
            let unlocked = ui_weak
                .upgrade()
                .is_some_and(|ui| ui.get_addons_community_adult_unlocked());
            let filters = filters_from_ui(&media_type, &sort, &category, &query, unlocked);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_addons_community_loading(true);
                ui.set_addons_community_available(false);
            }
            let revision = request_revision.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            tokio::spawn(fetch_and_project(
                ui_weak.clone(),
                filters,
                false,
                request_revision.clone(),
                revision,
            ));
        }
    });
    ui.on_addons_community_load_next({
        let ui_weak = ui_weak.clone();
        let request_revision = request_revision.clone();
        move |media_type, sort, category, query, page| {
            let unlocked = ui_weak
                .upgrade()
                .is_some_and(|ui| ui.get_addons_community_adult_unlocked());
            let mut filters = filters_from_ui(&media_type, &sort, &category, &query, unlocked);
            filters.page = u32::try_from(page).unwrap_or(1).max(1);
            tokio::spawn(fetch_and_project(
                ui_weak.clone(),
                filters,
                true,
                request_revision.clone(),
                request_revision.load(std::sync::atomic::Ordering::Acquire),
            ));
        }
    });
    let limiter = std::sync::Arc::new(crate::profiles::PinAttemptLimiter::default());
    ui.on_addons_community_unlock_adult({
        let ui_weak = ui_weak.clone();
        let request_revision = request_revision.clone();
        move |pin, media_type, sort, category, query| {
            let limiter = limiter.clone();
            let ui_weak = ui_weak.clone();
            let request_revision = request_revision.clone();
            let pin = pin.to_string();
            let filters = filters_from_ui(&media_type, &sort, &category, &query, true);
            tokio::spawn(async move {
                match crate::profiles::authorize_owner_pin(&pin, &limiter).await {
                    Ok(_) => {
                        let weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_addons_community_adult_unlocked(true);
                                ui.set_addons_community_owner_pin("".into());
                                ui.set_addons_community_loading(true);
                            }
                        });
                        let revision =
                            request_revision.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
                        fetch_and_project(ui_weak, filters, false, request_revision, revision)
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
    ui.on_addons_community_open_directory(|| {
        if let Err(error) = open::that(DIRECTORY_URL) {
            tracing::warn!(%error, "failed to open addon directory attribution URL");
        }
    });
}

fn filters_from_ui(
    media_type: &slint::SharedString,
    sort: &slint::SharedString,
    category: &slint::SharedString,
    query: &slint::SharedString,
    include_nsfw: bool,
) -> CommunityAddonFilters {
    let category = match category.as_str() {
        "Movies" => Some("movies"),
        "TV shows" => Some("tv+shows"),
        "Anime" => Some("anime"),
        "Live TV" => Some("live+tv"),
        "Debrid support" => Some("debrid+support"),
        "HTTP streams" => Some("http+streams"),
        "Metadata" => Some("metadata"),
        "Subtitles" => Some("subtitles"),
        "Torrents" => Some("torrents"),
        "Usenet" => Some("usenet"),
        "NSFW" => Some("nsfw"),
        _ => match media_type.as_str() {
            "Movie" => Some("movies"),
            "Series" => Some("tv+shows"),
            "Anime" => Some("anime"),
            "TV Channel" => Some("live+tv"),
            _ => None,
        },
    }
    .map(ToOwned::to_owned);
    let query = if query.trim().is_empty() {
        None
    } else {
        Some(query.trim().chars().take(200).collect())
    };
    CommunityAddonFilters {
        sort: match sort.as_str() {
            "Most starred" => AddonSort::Stars,
            "Newest" => AddonSort::Newest,
            _ => AddonSort::Rising,
        },
        category,
        query,
        include_nsfw,
        page: 0,
    }
}

async fn fetch_and_project(
    ui_weak: slint::Weak<crate::MainWindow>,
    filters: CommunityAddonFilters,
    append: bool,
    request_revision: Arc<std::sync::atomic::AtomicU64>,
    expected_revision: u64,
) {
    let result = default_client().fetch(&filters).await;
    let _ = slint::invoke_from_event_loop(move || {
        if request_revision.load(std::sync::atomic::Ordering::Acquire) != expected_revision {
            return;
        }
        let Some(ui) = ui_weak.upgrade() else { return };
        match result {
            Ok(page) => {
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
                        .iter()
                        .cloned()
                        .map(|addon| project_community_addon(addon, &ui_weak)),
                );
                ui.set_addons_community_list(slint::ModelRc::new(slint::VecModel::from(items)));
                ui.set_addons_community_available(true);
                ui.set_addons_community_has_next_page(page.has_next_page);
                ui.set_addons_community_loading(false);
            }
            Err(error) => {
                ui.set_addons_community_available(false);
                ui.set_addons_community_has_next_page(false);
                ui.set_addons_community_loading(false);
                ui.set_error_message(
                    format!("The Stremio Addons directory is unavailable: {error}").into(),
                );
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
        addon.categories.join(", ")
    } else {
        addon.resource_types.join(", ")
    };
    let score = addon.recent_stars.map_or_else(
        || {
            if addon.version.is_empty() {
                format!("★ {}", addon.stars)
            } else {
                format!("v{} · ★ {}", addon.version, addon.stars)
            }
        },
        |recent| format!("↑ {recent} today · ★ {}", addon.stars),
    );
    let background = addon
        .background
        .as_deref()
        .and_then(|value| url::Url::parse(value).ok());
    crate::AddonItem {
        id: addon.manifest_url.as_str().into(),
        name: addon.name.into(),
        version: score.into(),
        description: addon.description.into(),
        logo_url: addon.logo.unwrap_or_default().into(),
        logo: crate::image_cache::get_poster_image(&logo, ui_weak),
        is_installed: false,
        transport_url: addon.manifest_url.into(),
        configuration_url: addon.configuration_url.unwrap_or_default().into(),
        types_label: types_label.into(),
        configurable: addon.configurable,
        configuration_required: addon.configuration_required,
        supports_movie,
        supports_series,
        supports_anime,
        supports_tv,
        background_url: addon.background.unwrap_or_default().into(),
        background_image: crate::image_cache::get_poster_image(&background, ui_weak),
        adult: addon.adult,
        p2p: addon.p2p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addon(adult: bool) -> CommunityAddon {
        CommunityAddon {
            name: "Addon".to_owned(),
            description: "Description".to_owned(),
            manifest_url: "https://addon.example/manifest.json".to_owned(),
            logo: Some("https://addon.example/logo.png".to_owned()),
            background: Some("https://addon.example/background.jpg".to_owned()),
            version: "1.0.0".to_owned(),
            resource_types: vec!["movie".to_owned(), "series".to_owned()],
            categories: vec!["movies".to_owned()],
            stars: 42,
            recent_stars: Some(3),
            adult,
            p2p: false,
            configurable: true,
            configuration_required: false,
            configuration_url: Some("https://addon.example/configure".to_owned()),
        }
    }

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
        let addon = addon(true);
        assert!(!visible_for_profile(
            &addon,
            crate::profiles::ProfileRole::Kids
        ));
        assert!(visible_for_profile(
            &addon,
            crate::profiles::ProfileRole::Owner
        ));
    }

    #[test]
    fn oversized_community_response_is_rejected_before_deserialization() {
        let bytes = vec![b' '; MAX_RESPONSE_BYTES + 1];

        assert_eq!(
            parse_community_page(&bytes, &CommunityAddonFilters::default()),
            Err(CommunityAddonError::InvalidResponse)
        );
    }

    #[test]
    fn community_page_weight_includes_owned_strings_and_collections() {
        let page = CommunityAddonPage {
            addons: vec![addon(false)],
            has_next_page: false,
        };

        assert!(community_page_weight("cache-key", &page) > CACHE_ENTRY_OVERHEAD);
    }

    #[test]
    fn community_cache_uses_balanced_policy() {
        let client = CommunityAddonClient::new("https://addons.example/api/v0").expect("client");
        let policy = client.cache.policy();

        assert_eq!(policy.max_capacity(), Some(CACHE_CAPACITY_BYTES));
        assert_eq!(policy.time_to_live(), Some(CACHE_TTL));
    }

    #[test]
    fn directory_query_uses_documented_v0_parameters() {
        let base = url::Url::parse("https://stremio-addons.net/api/v0/").expect("base URL");
        let filters = CommunityAddonFilters {
            sort: AddonSort::Stars,
            category: Some("tv+shows".to_owned()),
            query: Some("torrent".to_owned()),
            include_nsfw: false,
            page: 2,
        };

        let url = build_request_url(&base, &filters).expect("request URL");
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(url.path(), "/api/v0/addons");
        assert_eq!(pairs.get("page").map(|value| value.as_ref()), Some("2"));
        assert_eq!(
            pairs.get("sort_by").map(|value| value.as_ref()),
            Some("stars")
        );
        assert_eq!(
            pairs.get("category").map(|value| value.as_ref()),
            Some("tv+shows")
        );
        assert_eq!(
            pairs.get("search").map(|value| value.as_ref()),
            Some("torrent")
        );
        assert_eq!(
            pairs.get("nsfw").map(|value| value.as_ref()),
            Some("exclude")
        );
    }

    #[test]
    fn rising_response_is_normalized_without_pagination() {
        let bytes = br#"{
            "addons": [{
                "manifestUrl": "https://addon.example/manifest.json",
                "manifest": {
                    "name": "Example",
                    "description": "Example addon",
                    "version": "1.2.3",
                    "logo": "https://addon.example/logo.png",
                    "types": ["movie"],
                    "behaviorHints": {
                        "configurable": true,
                        "configurationRequired": false
                    }
                },
                "stars": 25,
                "recentStars": 5,
                "categories": [{"name": "movies", "slug": "movies"}],
                "configureUrl": "https://addon.example/configure"
            }]
        }"#;

        let page = parse_community_page(bytes, &CommunityAddonFilters::default())
            .expect("valid API response");

        assert_eq!(page.addons.len(), 1);
        assert_eq!(page.addons[0].recent_stars, Some(5));
        assert_eq!(
            page.addons[0].configuration_url.as_deref(),
            Some("https://addon.example/configure")
        );
        assert!(!page.has_next_page);
    }

    #[test]
    fn untrusted_configuration_url_is_not_exposed() {
        let api = ApiAddon {
            manifest_url: "https://addon.example/manifest.json".to_owned(),
            manifest: ApiManifest {
                name: "Example".to_owned(),
                behavior_hints: ApiBehaviorHints {
                    configurable: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            stars: 0,
            recent_stars: None,
            categories: Vec::new(),
            configure_url: Some("https://evil.example/configure".to_owned()),
        };

        let addon = normalize_api_addon(api).expect("valid manifest URL");

        assert!(!addon.configurable);
        assert!(addon.configuration_url.is_none());
    }
}

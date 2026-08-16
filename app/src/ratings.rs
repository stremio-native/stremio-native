use std::{
    sync::{Arc, LazyLock, Mutex, OnceLock},
    time::Duration,
};

use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;

use crate::MainWindow;

static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36")
        .build()
        .unwrap_or_default()
});

static RATINGS_CACHE: LazyLock<Cache<String, Arc<MediaRatings>>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(500)
        .time_to_live(Duration::from_secs(60 * 60))
        .build()
});

static CURRENT_MEDIA_ID: OnceLock<Mutex<String>> = OnceLock::new();

#[derive(Clone, Debug, Default)]
pub struct MediaRatings {
    pub rotten_tomatoes: Option<String>,
    pub metacritic: Option<String>,
    pub letterboxd: Option<String>,
    pub anilist: Option<String>,
    pub kitsu: Option<String>,
}

pub fn clear_current_media() {
    if let Ok(mut id) = CURRENT_MEDIA_ID
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
    {
        id.clear();
    }
}

pub fn fetch_and_project(
    ui_weak: slint::Weak<MainWindow>,
    media_id: String,
    title: String,
    year: Option<String>,
    is_series: bool,
    is_anime: bool,
) {
    let current_slot = CURRENT_MEDIA_ID.get_or_init(|| Mutex::new(String::new()));
    if let Ok(mut current) = current_slot.lock() {
        *current = media_id.clone();
    }

    // Reset current rating UI immediately
    let weak_init = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak_init.upgrade() {
            ui.set_detail_rt_rating("".into());
            ui.set_detail_metacritic_rating("".into());
            ui.set_detail_letterboxd_rating("".into());
            ui.set_detail_anilist_rating("".into());
            ui.set_detail_kitsu_rating("".into());
        }
    });

    if title.trim().is_empty() {
        return;
    }

    tokio::spawn(async move {
        let cache_key = format!("{}:{}:{is_series}:{is_anime}", media_id, title);
        if let Some(cached) = RATINGS_CACHE.get(&cache_key).await {
            project_if_current(&ui_weak, &media_id, &cached);
            return;
        }

        let mut ratings = MediaRatings::default();

        let (ani_res, kit_res, rt_res, mc_res, lb_res) = tokio::join!(
            async {
                if is_anime {
                    fetch_anilist_rating(&title).await
                } else {
                    None
                }
            },
            async {
                if is_anime {
                    fetch_kitsu_rating(&title).await
                } else {
                    None
                }
            },
            fetch_rotten_tomatoes_rating(&title, year.as_deref(), is_series),
            fetch_metacritic_rating(&title, is_series),
            async {
                if !is_series {
                    fetch_letterboxd_rating(&title).await
                } else {
                    None
                }
            }
        );
        ratings.anilist = ani_res;
        ratings.kitsu = kit_res;
        ratings.rotten_tomatoes = rt_res;
        ratings.metacritic = mc_res;
        ratings.letterboxd = lb_res;

        let ratings_arc = Arc::new(ratings);
        RATINGS_CACHE.insert(cache_key, ratings_arc.clone()).await;
        project_if_current(&ui_weak, &media_id, &ratings_arc);
    });
}

fn project_if_current(ui_weak: &slint::Weak<MainWindow>, target_id: &str, ratings: &MediaRatings) {
    let current_id = CURRENT_MEDIA_ID
        .get_or_init(|| Mutex::new(String::new()))
        .lock()
        .map(|id| id.clone())
        .unwrap_or_default();

    if current_id != target_id {
        return;
    }

    let weak = ui_weak.clone();
    let rt = ratings.rotten_tomatoes.clone().unwrap_or_default();
    let mc = ratings.metacritic.clone().unwrap_or_default();
    let lb = ratings.letterboxd.clone().unwrap_or_default();
    let ani = ratings.anilist.clone().unwrap_or_default();
    let kit = ratings.kitsu.clone().unwrap_or_default();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_detail_rt_rating(rt.into());
            ui.set_detail_metacritic_rating(mc.into());
            ui.set_detail_letterboxd_rating(lb.into());
            ui.set_detail_anilist_rating(ani.into());
            ui.set_detail_kitsu_rating(kit.into());
        }
    });
}

// -----------------------------------------------------------------------------
// Slug Helpers
// -----------------------------------------------------------------------------
fn sanitize_for_slug(title: &str) -> String {
    let title = title.to_lowercase();
    regex_lite_or_str(&title)
}

fn regex_lite_or_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn to_rt_slugs(title: &str, year: Option<&str>) -> Vec<String> {
    let mut base = String::with_capacity(title.len());
    let mut last_was_und = false;
    for c in title.chars() {
        if c.is_alphanumeric() {
            base.push(c.to_ascii_lowercase());
            last_was_und = false;
        } else if !last_was_und {
            base.push('_');
            last_was_und = true;
        }
    }
    let clean_base = base.trim_matches('_').to_string();
    let mut slugs = Vec::new();
    if let Some(yr) = year.filter(|y| !y.is_empty()) {
        slugs.push(format!("{clean_base}_{yr}"));
    }
    slugs.push(clean_base);
    slugs
}

// -----------------------------------------------------------------------------
// AniList (GraphQL)
// -----------------------------------------------------------------------------
#[derive(Deserialize)]
struct AniListResponse {
    data: Option<AniListData>,
}

#[derive(Deserialize)]
struct AniListData {
    #[serde(rename = "Media")]
    media: Option<AniListMedia>,
}

#[derive(Deserialize)]
struct AniListMedia {
    #[serde(rename = "averageScore")]
    average_score: Option<i32>,
}

async fn fetch_anilist_rating(title: &str) -> Option<String> {
    let query = "query ($search: String) { Media (search: $search, type: ANIME) { averageScore } }";
    let body = serde_json::json!({
        "query": query,
        "variables": { "search": title }
    });

    let resp = HTTP_CLIENT
        .post("https://graphql.anilist.co")
        .json(&body)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let data: AniListResponse = resp.json().await.ok()?;
    let score = data.data?.media?.average_score?;
    Some(format!("{score}%"))
}

// -----------------------------------------------------------------------------
// Kitsu (JSON API)
// -----------------------------------------------------------------------------
#[derive(Deserialize)]
struct KitsuResponse {
    data: Option<Vec<KitsuItem>>,
}

#[derive(Deserialize)]
struct KitsuItem {
    attributes: Option<KitsuAttrs>,
}

#[derive(Deserialize)]
struct KitsuAttrs {
    #[serde(rename = "averageRating")]
    average_rating: Option<String>,
}

async fn fetch_kitsu_rating(title: &str) -> Option<String> {
    let q = percent_encoding::utf8_percent_encode(title, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    let url = format!("https://kitsu.io/api/edge/anime?filter[text]={q}&page[limit]=1");

    let resp = HTTP_CLIENT
        .get(&url)
        .header("Accept", "application/vnd.api+json")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let data: KitsuResponse = resp.json().await.ok()?;
    let items = data.data?;
    let avg = items
        .first()?
        .attributes
        .as_ref()?
        .average_rating
        .as_ref()?;
    let float_val: f64 = avg.parse().ok()?;
    Some(format!("{:.1}", float_val / 10.0))
}

// -----------------------------------------------------------------------------
// Rotten Tomatoes
// -----------------------------------------------------------------------------
async fn fetch_rotten_tomatoes_rating(
    title: &str,
    year: Option<&str>,
    is_tv: bool,
) -> Option<String> {
    let prefix = if is_tv { "tv" } else { "m" };
    let slugs = to_rt_slugs(title, year);

    for slug in slugs {
        let url = format!("https://www.rottentomatoes.com/{prefix}/{slug}");
        if let Ok(resp) = HTTP_CLIENT.get(&url).send().await {
            if !resp.status().is_success() {
                continue;
            }
            if let Ok(html) = resp.text().await
                && let Some(rating) = parse_json_ld_rating(&html)
            {
                return Some(format!("{rating}%"));
            }
        }
    }
    None
}

// -----------------------------------------------------------------------------
// Metacritic
// -----------------------------------------------------------------------------
async fn fetch_metacritic_rating(title: &str, is_tv: bool) -> Option<String> {
    let prefix = if is_tv { "tv" } else { "movie" };
    let slug = sanitize_for_slug(title);
    let url = format!("https://www.metacritic.com/{prefix}/{slug}/");

    let resp = HTTP_CLIENT.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let html = resp.text().await.ok()?;
    if let Some(val) = parse_json_ld_rating(&html) {
        return Some(val);
    }

    // Fallback: regex on c-siteReviewScore
    if let Some(pos) = html.find("c-siteReviewScore") {
        let slice = &html[pos..pos + 120.min(html.len() - pos)];
        if let Some(start) = slice.find("<span>") {
            let num_slice = &slice[start + 6..];
            if let Some(end) = num_slice.find("</span>") {
                let score = num_slice[..end].trim();
                if score.chars().all(|c| c.is_ascii_digit()) && !score.is_empty() {
                    return Some(score.to_string());
                }
            }
        }
    }

    None
}

// -----------------------------------------------------------------------------
// Letterboxd
// -----------------------------------------------------------------------------
async fn fetch_letterboxd_rating(title: &str) -> Option<String> {
    let slug = sanitize_for_slug(title);
    let url = format!("https://letterboxd.com/film/{slug}/");

    let resp = HTTP_CLIENT.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let html = resp.text().await.ok()?;
    if let Some(val) = parse_json_ld_rating(&html) {
        if let Ok(float_val) = val.parse::<f64>() {
            return Some(format!("{:.1}", float_val));
        }
        return Some(val);
    }

    None
}

// -----------------------------------------------------------------------------
// Shared JSON-LD Extractor
// -----------------------------------------------------------------------------
fn parse_json_ld_rating(html: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(start) = html[search_from..].find("<script type=\"application/ld+json\">") {
        let script_start = search_from + start + 35;
        if let Some(end) = html[script_start..].find("</script>") {
            let script_content = &html[script_start..script_start + end];
            search_from = script_start + end + 9;

            let clean = script_content.trim();
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(clean)
                && let Some(val) = json_val.pointer("/aggregateRating/ratingValue")
            {
                if let Some(str_val) = val.as_str() {
                    return Some(str_val.to_string());
                }
                if let Some(num_val) = val.as_f64() {
                    return Some(format!("{num_val}"));
                }
                if let Some(int_val) = val.as_i64() {
                    return Some(format!("{int_val}"));
                }
            }
        } else {
            break;
        }
    }
    None
}

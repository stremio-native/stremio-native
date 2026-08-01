use crate::MainWindow;
use crate::config::AppConfig;
use rodio::{Decoder, MixerDeviceSink, Player, Source};
use slint::ComponentHandle;
use std::io::Cursor;
use std::sync::Mutex;

static ONBOARDING_AUDIO_BYTES: &[u8] = include_bytes!("../../ui/assets/audio/onboarding.mp3");

struct OnboardingAudioPlayer {
    _stream: MixerDeviceSink,
    player: Player,
}

static AUDIO_PLAYER: Mutex<Option<OnboardingAudioPlayer>> = Mutex::new(None);

pub fn play_music() {
    std::thread::spawn(move || {
        let cursor = Cursor::new(ONBOARDING_AUDIO_BYTES);

        let stream = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = ?e, "Could not initialize rodio audio output stream");
                return;
            }
        };

        let player = Player::connect_new(stream.mixer());

        let source = match Decoder::try_from(cursor) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = ?e, "Could not decode embedded onboarding mp3 audio");
                return;
            }
        };

        player.append(source.repeat_infinite());
        player.set_volume(0.3);
        player.play();

        let mut lock = AUDIO_PLAYER.lock().unwrap();
        *lock = Some(OnboardingAudioPlayer {
            _stream: stream,
            player,
        });
        tracing::info!(
            "Onboarding background music playing via Rodio from embedded binary assets."
        );
    });
}

pub fn stop_music() {
    let mut lock = AUDIO_PLAYER.lock().unwrap();
    if let Some(audio_player) = lock.take() {
        audio_player.player.stop();
        tracing::info!("Onboarding background music stopped.");
    }
}

pub fn fetch_github_changelog(ui: &MainWindow) {
    let ui_weak = ui.as_weak();
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .user_agent("stremio-native")
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        let mut raw_changelog = String::new();

        // 1. Try fetching releases from GitHub API
        let releases_url = "https://api.github.com/repos/stremio-native/stremio-native/releases";
        if let Ok(res) = client
            .get(releases_url)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            && res.status().is_success()
            && let Ok(releases) = res.json::<Vec<serde_json::Value>>().await
        {
            for rel in releases.iter().take(5) {
                let tag = rel["tag_name"].as_str().unwrap_or("");
                let name = rel["name"].as_str().unwrap_or(tag);
                let body = rel["body"].as_str().unwrap_or("");
                if !name.is_empty() {
                    raw_changelog.push_str(&format!("# {}\n\n", name));
                }
                raw_changelog.push_str(body);
                raw_changelog.push_str("\n\n");
            }
        }

        // 2. Fallback: If no releases published yet, fetch recent git commits from GitHub API
        if raw_changelog.trim().is_empty() {
            let commits_url =
                "https://api.github.com/repos/stremio-native/stremio-native/commits?per_page=12";
            if let Ok(res) = client
                .get(commits_url)
                .header("Accept", "application/vnd.github.v3+json")
                .send()
                .await
                && res.status().is_success()
                && let Ok(commits) = res.json::<Vec<serde_json::Value>>().await
            {
                raw_changelog.push_str(&format!(
                    "### v{} - Recent GitHub Change History\n\n",
                    env!("CARGO_PKG_VERSION")
                ));
                for item in commits {
                    let sha = item["sha"].as_str().unwrap_or("").get(..7).unwrap_or("");
                    let msg = item["commit"]["message"].as_str().unwrap_or("");
                    let first_line = msg.lines().next().unwrap_or("").trim();
                    if !first_line.is_empty() {
                        raw_changelog.push_str(&format!("* [`{}`] {}\n", sha, first_line));
                    }
                }
            }
        }

        let trimmed = raw_changelog.trim();
        if !trimmed.is_empty() {
            let styled = slint::StyledText::from_markdown(trimmed)
                .unwrap_or_else(|_| slint::StyledText::from_plain_text(trimmed));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_onboarding_changelog_text(styled);
                }
            });
        }
    });
}

pub fn setup(ui: &MainWindow, config: &AppConfig) {
    fetch_github_changelog(ui);

    let force_onboarding = std::env::args().any(|arg| {
        arg == "--force-onboarding" || arg == "--onboarding" || arg == "--test-onboarding"
    });

    // Show onboarding modal on first launch or when forced via CLI argument
    if force_onboarding || !config.onboarding_completed {
        tracing::info!(
            force = force_onboarding,
            "Triggering onboarding wizard popup"
        );
        ui.set_onboarding_dialog_open(true);
        play_music();
    }

    // Bind onboarding completed callback
    ui.on_onboarding_completed({
        move || {
            stop_music();
            let mut cfg = crate::config::load_config();
            cfg.onboarding_completed = true;
            crate::config::save_config(&cfg);
            tracing::info!("Onboarding completed and saved to configuration.");
        }
    });
}

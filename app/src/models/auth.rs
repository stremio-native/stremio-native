use crate::AppModel;
use crate::MainWindow;
use core_env::DesktopEnv;
use serde::Deserialize;
use slint::ComponentHandle;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use stremio_core::{
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionCtx},
    },
    types::{api::AuthRequest, profile::GDPRConsent},
};
use tokio::time::{Duration, sleep};

const STREMIO_URL: &str = "https://www.strem.io";
const MAX_SOCIAL_LOGIN_TRIES: usize = 25;

fn is_valid_email(value: &str) -> bool {
    let value = value.trim();
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && !value.chars().any(char::is_whitespace)
}

#[derive(Deserialize)]
struct FacebookLoginResponse {
    user: FacebookLoginUser,
}

#[derive(Deserialize)]
struct FacebookLoginUser {
    email: String,
    #[serde(rename = "fbLoginToken")]
    login_token: String,
}

#[derive(Deserialize)]
struct AppleLoginResponse {
    user: AppleLoginUser,
}

#[derive(Deserialize)]
struct AppleLoginUser {
    token: String,
    sub: String,
    email: String,
    #[serde(default)]
    name: String,
}

fn set_auth_error(ui_weak: slint::Weak<MainWindow>, message: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_loading(false);
            ui.set_error_message(message.into());
        }
    });
}

pub fn setup(ui: &MainWindow, runtime: &Arc<Runtime<DesktopEnv, AppModel>>) {
    let ui_weak = ui.as_weak();
    let social_login_generation = Arc::new(AtomicUsize::new(0));
    let http_client = reqwest::Client::new();

    // Login callback
    ui.on_submit_login({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |email, password| {
            if !is_valid_email(email.as_str()) {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_error_message("Please enter a valid email address".into());
                }
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_loading(true);
                ui.set_error_message("".into());
            }
            let rt = runtime.clone();
            let email = email.to_string();
            let password = password.to_string();
            tokio::spawn(async move {
                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::Authenticate(AuthRequest::Login {
                        email,
                        password,
                        facebook: false,
                    })),
                });
            });
        }
    });

    // Register callback
    ui.on_submit_register({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move |email, password, tos, privacy, marketing| {
            if !is_valid_email(email.as_str()) {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_error_message("Please enter a valid email address".into());
                }
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_loading(true);
                ui.set_error_message("".into());
            }
            let rt = runtime.clone();
            let email = email.to_string();
            let password = password.to_string();
            tokio::spawn(async move {
                let gdpr_consent = GDPRConsent {
                    tos,
                    privacy,
                    marketing,
                    from: Some("desktop".to_string()),
                };
                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::Authenticate(AuthRequest::Register {
                        email,
                        password,
                        gdpr_consent,
                    })),
                });
            });
        }
    });

    ui.on_facebook_login({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let http_client = http_client.clone();
        let social_login_generation = social_login_generation.clone();
        move || {
            let attempt = social_login_generation.fetch_add(1, Ordering::SeqCst) + 1;
            let state = uuid::Uuid::new_v4().simple().to_string();
            let login_url = format!("{STREMIO_URL}/login-fb/{state}");

            if let Some(ui) = ui_weak.upgrade() {
                ui.set_loading(true);
                ui.set_error_message("".into());
            }

            if let Err(error) = open::that(&login_url) {
                set_auth_error(
                    ui_weak.clone(),
                    format!("Could not open Facebook login: {error}"),
                );
                return;
            }

            let runtime = runtime.clone();
            let ui_weak = ui_weak.clone();
            let http_client = http_client.clone();
            let social_login_generation = social_login_generation.clone();
            tokio::spawn(async move {
                let credentials_url = format!("{STREMIO_URL}/login-fb-get-acc/{state}");
                for _ in 0..MAX_SOCIAL_LOGIN_TRIES {
                    sleep(Duration::from_secs(1)).await;
                    if social_login_generation.load(Ordering::SeqCst) != attempt {
                        return;
                    }

                    let response = match http_client.get(&credentials_url).send().await {
                        Ok(response) if response.status().is_success() => response,
                        _ => continue,
                    };
                    let response = match response.json::<FacebookLoginResponse>().await {
                        Ok(response) => response,
                        Err(_) => continue,
                    };

                    runtime.dispatch(RuntimeAction {
                        field: None,
                        action: Action::Ctx(ActionCtx::Authenticate(AuthRequest::Login {
                            email: response.user.email,
                            password: response.user.login_token,
                            facebook: true,
                        })),
                    });
                    return;
                }

                if social_login_generation.load(Ordering::SeqCst) == attempt {
                    set_auth_error(
                        ui_weak,
                        "Failed to authenticate with Facebook. Please try again.".to_owned(),
                    );
                }
            });
        }
    });

    ui.on_apple_login({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        let http_client = http_client.clone();
        let social_login_generation = social_login_generation.clone();
        move || {
            let attempt = social_login_generation.fetch_add(1, Ordering::SeqCst) + 1;
            let state = uuid::Uuid::new_v4().simple().to_string();
            let login_url = format!("{STREMIO_URL}/login-apple/{state}");

            if let Some(ui) = ui_weak.upgrade() {
                ui.set_loading(true);
                ui.set_error_message("".into());
            }

            if let Err(error) = open::that(&login_url) {
                set_auth_error(
                    ui_weak.clone(),
                    format!("Could not open Apple login: {error}"),
                );
                return;
            }

            let runtime = runtime.clone();
            let ui_weak = ui_weak.clone();
            let http_client = http_client.clone();
            let social_login_generation = social_login_generation.clone();
            tokio::spawn(async move {
                let credentials_url = format!("{STREMIO_URL}/login-apple-get-acc/{state}");
                for _ in 0..MAX_SOCIAL_LOGIN_TRIES {
                    sleep(Duration::from_secs(2)).await;
                    if social_login_generation.load(Ordering::SeqCst) != attempt {
                        return;
                    }

                    let response = match http_client.get(&credentials_url).send().await {
                        Ok(response) if response.status().is_success() => response,
                        _ => continue,
                    };
                    let response = match response.json::<AppleLoginResponse>().await {
                        Ok(response) => response,
                        Err(_) => continue,
                    };

                    runtime.dispatch(RuntimeAction {
                        field: None,
                        action: Action::Ctx(ActionCtx::Authenticate(AuthRequest::Apple {
                            token: response.user.token,
                            sub: response.user.sub,
                            email: response.user.email,
                            name: response.user.name,
                        })),
                    });
                    return;
                }

                if social_login_generation.load(Ordering::SeqCst) == attempt {
                    set_auth_error(
                        ui_weak,
                        "Failed to authenticate with Apple. Please try again.".to_owned(),
                    );
                }
            });
        }
    });

    ui.on_cancel_auth({
        let ui_weak = ui_weak.clone();
        let social_login_generation = social_login_generation.clone();
        move || {
            social_login_generation.fetch_add(1, Ordering::SeqCst);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_loading(false);
            }
        }
    });

    ui.on_request_password_reset({
        move |email| {
            if !is_valid_email(email.as_str()) {
                tracing::warn!(%email, "ignored password reset for an invalid email address");
                return;
            }
            let mut reset_url = match url::Url::parse(&format!("{STREMIO_URL}/reset-password/")) {
                Ok(url) => url,
                Err(error) => {
                    tracing::error!(%error, "invalid Stremio password-reset base URL");
                    return;
                }
            };
            if let Ok(mut path) = reset_url.path_segments_mut() {
                path.push(email.as_str());
            }
            if let Err(error) = open::that(reset_url.as_str()) {
                tracing::warn!(%error, "could not open the password-reset URL");
            }
        }
    });

    // Logout callback
    ui.on_logout({
        let runtime = runtime.clone();
        let ui_weak = ui_weak.clone();
        move || {
            let rt = runtime.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                rt.dispatch(RuntimeAction {
                    field: None,
                    action: Action::Ctx(ActionCtx::Logout),
                });
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_username("".into());
                    }
                });
            });
        }
    });

    // Guest login callback
    ui.on_guest_login({
        let ui_weak = ui_weak.clone();
        move || {
            let ui_weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_username("Guest".into());
                    ui.set_avatar_letter("G".into());
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::is_valid_email;

    #[test]
    fn validates_email_shape_before_authentication() {
        assert!(is_valid_email("viewer@example.com"));
        assert!(is_valid_email("viewer@localhost"));
        assert!(!is_valid_email("viewer"));
        assert!(!is_valid_email("@example.com"));
        assert!(!is_valid_email("viewer @example.com"));
    }
}

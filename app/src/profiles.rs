use std::{collections::HashMap, str::FromStr, sync::Mutex, time::Duration};

use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ProfileError> {
        let value = value.into();
        uuid::Uuid::parse_str(&value).map_err(|_| ProfileError::InvalidProfileId)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileRole {
    Owner,
    Standard,
    Kids,
}

impl ProfileRole {
    fn as_db(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Standard => "standard",
            Self::Kids => "kids",
        }
    }
}

impl FromStr for ProfileRole {
    type Err = ProfileError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "owner" => Ok(Self::Owner),
            "standard" => Ok(Self::Standard),
            "kids" => Ok(Self::Kids),
            _ => Err(ProfileError::InvalidRole),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalProfile {
    pub id: ProfileId,
    pub name: String,
    pub avatar: Option<String>,
    pub role: ProfileRole,
    pub has_pin: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParentalDecision {
    Allow,
    RequireOwnerPin,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProfileError {
    #[error("profile identifier is invalid")]
    InvalidProfileId,
    #[error("profile role is invalid")]
    InvalidRole,
    #[error("profile name must contain between 1 and 48 characters")]
    InvalidName,
    #[error("PIN must contain between 4 and 12 digits")]
    InvalidPin,
    #[error("PIN is incorrect")]
    IncorrectPin,
    #[error("PIN verification is temporarily rate limited")]
    PinRateLimited(Duration),
    #[error("at least one Owner profile must remain")]
    LastOwner,
    #[error("profile was not found")]
    NotFound,
    #[error("the active profile cannot be deleted; switch profiles first")]
    ActiveProfile,
    #[error("profile database operation failed: {0}")]
    Database(String),
}

impl From<turso::Error> for ProfileError {
    fn from(error: turso::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Default)]
pub struct PinAttemptLimiter {
    attempts: Mutex<HashMap<ProfileId, FailedAttempts>>,
}

#[derive(Clone, Debug)]
struct FailedAttempts {
    count: u32,
    retry_at: std::time::Instant,
}

impl PinAttemptLimiter {
    fn preflight(&self, profile_id: &ProfileId) -> Result<(), ProfileError> {
        let attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(failed) = attempts.get(profile_id) {
            let now = std::time::Instant::now();
            if failed.retry_at > now {
                return Err(ProfileError::PinRateLimited(failed.retry_at - now));
            }
        }
        Ok(())
    }

    fn failed(&self, profile_id: &ProfileId) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let failed = attempts
            .entry(profile_id.clone())
            .or_insert(FailedAttempts {
                count: 0,
                retry_at: std::time::Instant::now(),
            });
        failed.count = failed.count.saturating_add(1);
        let delay = match failed.count {
            0..=2 => Duration::ZERO,
            3 => Duration::from_secs(1),
            4 => Duration::from_secs(5),
            5 => Duration::from_secs(30),
            _ => Duration::from_secs(300),
        };
        failed.retry_at = std::time::Instant::now() + delay;
    }

    fn succeeded(&self, profile_id: &ProfileId) {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(profile_id);
    }
}

pub async fn list_profiles() -> Result<Vec<LocalProfile>, ProfileError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    let mut rows = conn
        .query(
            "SELECT id, name, avatar, role, pin_hash IS NOT NULL, created_at, updated_at
             FROM local_profiles ORDER BY created_at, id",
            (),
        )
        .await?;
    let mut profiles = Vec::new();
    while let Some(row) = rows.next().await? {
        profiles.push(LocalProfile {
            id: ProfileId::parse(row.get::<String>(0)?)?,
            name: row.get(1)?,
            avatar: row.get(2)?,
            role: row.get::<String>(3)?.parse()?,
            has_pin: row.get::<i64>(4)? != 0,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
        });
    }
    Ok(profiles)
}

pub async fn active_profile_id() -> Result<ProfileId, ProfileError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    let mut rows = conn
        .query(
            "SELECT value FROM app_state WHERE key = 'active_profile_id'",
            (),
        )
        .await?;
    let profile_id = rows
        .next()
        .await?
        .map(|row| row.get::<String>(0))
        .transpose()?
        .ok_or(ProfileError::NotFound)?;
    ProfileId::parse(profile_id)
}

pub async fn set_active_profile(profile_id: &ProfileId) -> Result<(), ProfileError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    let changed = conn
        .execute(
            "UPDATE app_state SET value = ? WHERE key = 'active_profile_id'
             AND EXISTS(SELECT 1 FROM local_profiles WHERE id = ?)",
            (profile_id.as_str(), profile_id.as_str()),
        )
        .await?;
    if changed == 0 {
        return Err(ProfileError::NotFound);
    }
    Ok(())
}

pub async fn setting(profile_id: &ProfileId, key: &str) -> Result<Option<String>, ProfileError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    let mut rows = conn
        .query(
            "SELECT value FROM profile_settings WHERE profile_id = ? AND key = ?",
            (profile_id.as_str(), key),
        )
        .await?;
    rows.next()
        .await?
        .map(|row| row.get::<String>(0))
        .transpose()
        .map_err(Into::into)
}

pub async fn set_setting(
    profile_id: &ProfileId,
    key: &str,
    value: &str,
) -> Result<(), ProfileError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    conn.execute(
        "INSERT INTO profile_settings(profile_id, key, value) VALUES (?, ?, ?)
         ON CONFLICT(profile_id, key) DO UPDATE SET value = excluded.value",
        (profile_id.as_str(), key, value),
    )
    .await?;
    Ok(())
}

pub async fn delete_setting(profile_id: &ProfileId, key: &str) -> Result<(), ProfileError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    conn.execute(
        "DELETE FROM profile_settings WHERE profile_id = ? AND key = ?",
        (profile_id.as_str(), key),
    )
    .await?;
    Ok(())
}

pub async fn create_profile(
    name: &str,
    role: ProfileRole,
    avatar: Option<&str>,
) -> Result<LocalProfile, ProfileError> {
    let name = validated_name(name)?;
    let id = ProfileId::new();
    let now = chrono::Utc::now().timestamp();
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    conn.execute(
        "INSERT INTO local_profiles(id, name, avatar, role, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        (id.as_str(), name.as_str(), avatar, role.as_db(), now, now),
    )
    .await?;
    Ok(LocalProfile {
        id,
        name,
        avatar: avatar.map(ToOwned::to_owned),
        role,
        has_pin: false,
        created_at: now,
        updated_at: now,
    })
}

pub async fn delete_profile(profile_id: &ProfileId) -> Result<(), ProfileError> {
    let mut conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    let transaction = conn.transaction().await?;
    let mut active_rows = transaction
        .query(
            "SELECT value FROM app_state WHERE key = 'active_profile_id'",
            (),
        )
        .await?;
    let is_active = active_rows
        .next()
        .await?
        .map(|row| row.get::<String>(0))
        .transpose()?
        .is_some_and(|active| active == profile_id.as_str());
    drop(active_rows);
    if is_active {
        return Err(ProfileError::ActiveProfile);
    }
    let mut rows = transaction
        .query(
            "SELECT role FROM local_profiles WHERE id = ?",
            [profile_id.as_str()],
        )
        .await?;
    let role = rows
        .next()
        .await?
        .map(|row| row.get::<String>(0))
        .transpose()?
        .ok_or(ProfileError::NotFound)?;
    drop(rows);
    if role == "owner" {
        let mut owner_rows = transaction
            .query(
                "SELECT COUNT(*) FROM local_profiles WHERE role = 'owner'",
                (),
            )
            .await?;
        let owner_count = owner_rows
            .next()
            .await?
            .map(|row| row.get::<i64>(0))
            .transpose()?
            .unwrap_or_default();
        drop(owner_rows);
        if owner_count <= 1 {
            return Err(ProfileError::LastOwner);
        }
    }
    transaction
        .execute(
            "DELETE FROM local_profiles WHERE id = ?",
            [profile_id.as_str()],
        )
        .await?;
    transaction.commit().await?;
    Ok(())
}

pub async fn authorize_owner_pin(
    pin: &str,
    limiter: &PinAttemptLimiter,
) -> Result<ProfileId, ProfileError> {
    validate_pin(pin)?;
    let owners = list_profiles()
        .await?
        .into_iter()
        .filter(|profile| profile.role == ProfileRole::Owner)
        .collect::<Vec<_>>();
    let protected = owners
        .iter()
        .filter(|profile| profile.has_pin)
        .collect::<Vec<_>>();
    if protected.is_empty() {
        let owner = owners.first().ok_or(ProfileError::LastOwner)?;
        set_pin(&owner.id, pin).await?;
        verify_pin(&owner.id, pin, limiter).await?;
        return Ok(owner.id.clone());
    }
    let mut rate_limit = None;
    for owner in protected {
        match verify_pin(&owner.id, pin, limiter).await {
            Ok(()) => return Ok(owner.id.clone()),
            Err(error @ ProfileError::PinRateLimited(_)) => rate_limit = Some(error),
            Err(ProfileError::IncorrectPin) | Err(ProfileError::InvalidPin) => {}
            Err(error) => return Err(error),
        }
    }
    Err(rate_limit.unwrap_or(ProfileError::IncorrectPin))
}

pub async fn set_pin(profile_id: &ProfileId, pin: &str) -> Result<(), ProfileError> {
    validate_pin(pin)?;
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map_err(|_| ProfileError::InvalidPin)?
        .to_string();
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    let changed = conn
        .execute(
            "UPDATE local_profiles SET pin_hash = ?, updated_at = ? WHERE id = ?",
            (hash, chrono::Utc::now().timestamp(), profile_id.as_str()),
        )
        .await?;
    if changed == 0 {
        return Err(ProfileError::NotFound);
    }
    Ok(())
}

pub async fn verify_pin(
    profile_id: &ProfileId,
    pin: &str,
    limiter: &PinAttemptLimiter,
) -> Result<(), ProfileError> {
    limiter.preflight(profile_id)?;
    validate_pin(pin)?;
    let conn = crate::db::get_conn()
        .await
        .map_err(|error| ProfileError::Database(error.to_string()))?;
    let mut rows = conn
        .query(
            "SELECT pin_hash FROM local_profiles WHERE id = ?",
            [profile_id.as_str()],
        )
        .await?;
    let hash = rows
        .next()
        .await?
        .map(|row| row.get::<Option<String>>(0))
        .transpose()?
        .flatten()
        .ok_or(ProfileError::IncorrectPin)?;
    drop(rows);
    drop(conn);
    let parsed = PasswordHash::new(&hash).map_err(|_| ProfileError::IncorrectPin)?;
    if Argon2::default()
        .verify_password(pin.as_bytes(), &parsed)
        .is_ok()
    {
        limiter.succeeded(profile_id);
        Ok(())
    } else {
        limiter.failed(profile_id);
        Err(ProfileError::IncorrectPin)
    }
}

pub fn parental_decision(role: ProfileRole, rating: Option<&str>) -> ParentalDecision {
    if role != ProfileRole::Kids {
        return ParentalDecision::Allow;
    }
    let known_family_rating = rating.is_some_and(|rating| {
        matches!(
            rating.trim().to_ascii_uppercase().as_str(),
            "G" | "TV-G" | "TV-Y" | "TV-Y7" | "U" | "PG" | "TV-PG" | "7" | "12"
        )
    });
    if known_family_rating {
        ParentalDecision::Allow
    } else {
        ParentalDecision::RequireOwnerPin
    }
}

fn validated_name(name: &str) -> Result<String, ProfileError> {
    let name = name.trim();
    let length = name.chars().count();
    if !(1..=48).contains(&length) {
        return Err(ProfileError::InvalidName);
    }
    Ok(name.to_owned())
}

fn validate_pin(pin: &str) -> Result<(), ProfileError> {
    if (4..=12).contains(&pin.len()) && pin.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(ProfileError::InvalidPin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kids_require_override_for_unknown_or_adult_ratings() {
        assert_eq!(
            parental_decision(ProfileRole::Kids, Some("TV-Y7")),
            ParentalDecision::Allow
        );
        assert_eq!(
            parental_decision(ProfileRole::Kids, None),
            ParentalDecision::RequireOwnerPin
        );
        assert_eq!(
            parental_decision(ProfileRole::Kids, Some("R")),
            ParentalDecision::RequireOwnerPin
        );
    }

    #[test]
    fn pin_validation_rejects_non_digits_and_weak_lengths() {
        assert_eq!(validate_pin("123"), Err(ProfileError::InvalidPin));
        assert_eq!(validate_pin("12a4"), Err(ProfileError::InvalidPin));
        assert_eq!(validate_pin("1234"), Ok(()));
    }
}

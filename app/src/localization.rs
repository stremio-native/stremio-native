use std::borrow::Cow;

use chrono::{DateTime, Datelike, Utc};
use fluent_bundle::{FluentArgs, FluentBundle, FluentResource, FluentValue};
use unic_langid::LanguageIdentifier;

const ENGLISH: &str = include_str!("../i18n/en.ftl");
const PORTUGUESE: &str = include_str!("../i18n/pt.ftl");
const ARABIC: &str = include_str!("../i18n/ar.ftl");

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageKey {
    AppName,
    DownloadsTitle,
    DownloadsEmpty,
    DownloadCount,
    ProfilesTitle,
    ProfileVaultLocked,
    ProfileOwner,
    ProfileStandard,
    ProfileKids,
    StreamRankingSmart,
    StreamRankingQuality,
    StreamRankingSmallest,
    StreamRankingSeeders,
    StreamRankingOriginal,
    PlayerRetrying,
    PlayerCaptureSaved,
    ActionRetry,
    ActionCancel,
    ActionReveal,
    ActionSelectProfile,
}

impl MessageKey {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AppName => "app-name",
            Self::DownloadsTitle => "downloads-title",
            Self::DownloadsEmpty => "downloads-empty",
            Self::DownloadCount => "download-count",
            Self::ProfilesTitle => "profiles-title",
            Self::ProfileVaultLocked => "profile-vault-locked",
            Self::ProfileOwner => "profile-owner",
            Self::ProfileStandard => "profile-standard",
            Self::ProfileKids => "profile-kids",
            Self::StreamRankingSmart => "stream-ranking-smart",
            Self::StreamRankingQuality => "stream-ranking-quality",
            Self::StreamRankingSmallest => "stream-ranking-smallest",
            Self::StreamRankingSeeders => "stream-ranking-seeders",
            Self::StreamRankingOriginal => "stream-ranking-original",
            Self::PlayerRetrying => "player-retrying",
            Self::PlayerCaptureSaved => "player-capture-saved",
            Self::ActionRetry => "action-retry",
            Self::ActionCancel => "action-cancel",
            Self::ActionReveal => "action-reveal",
            Self::ActionSelectProfile => "action-select-profile",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextDirection {
    LeftToRight,
    RightToLeft,
}

pub struct Localizer {
    locale: LanguageIdentifier,
    primary: FluentBundle<FluentResource>,
    fallback: FluentBundle<FluentResource>,
}

impl Localizer {
    pub fn new(locale: &str) -> Self {
        let fallback_locale: LanguageIdentifier = "en-US".parse().expect("valid locale");
        let locale: LanguageIdentifier = locale.parse().unwrap_or_else(|_| fallback_locale.clone());
        let (resource, supported_locale) = match locale.language.as_str() {
            "pt" => (PORTUGUESE, "pt-BR"),
            "ar" => (ARABIC, "ar"),
            _ => (ENGLISH, "en-US"),
        };
        Self {
            locale,
            primary: bundle(supported_locale, resource),
            fallback: bundle("en-US", ENGLISH),
        }
    }

    pub fn direction(&self) -> TextDirection {
        if self.locale.language.as_str() == "ar" {
            TextDirection::RightToLeft
        } else {
            TextDirection::LeftToRight
        }
    }

    pub fn text(&self, key: MessageKey, args: Option<&FluentArgs<'_>>) -> String {
        format_message(&self.primary, key, args)
            .or_else(|| format_message(&self.fallback, key, args))
            .unwrap_or_else(|| key.as_str().to_owned())
    }

    pub fn count(&self, key: MessageKey, count: u64) -> String {
        let mut args = FluentArgs::new();
        args.set("count", FluentValue::from(count as i64));
        self.text(key, Some(&args))
    }

    pub fn format_number(&self, value: i64) -> String {
        let negative = value.is_negative();
        let digits = value.unsigned_abs().to_string();
        let separator = if self.locale.language.as_str() == "pt" {
            '.'
        } else {
            ','
        };
        let mut formatted = String::new();
        for (index, digit) in digits.chars().rev().enumerate() {
            if index > 0 && index.is_multiple_of(3) {
                formatted.push(separator);
            }
            formatted.push(digit);
        }
        let mut formatted = formatted.chars().rev().collect::<String>();
        if negative {
            formatted.insert(0, '-');
        }
        if self.locale.language.as_str() == "ar" {
            localize_arabic_digits(&formatted)
        } else {
            formatted
        }
    }

    pub fn format_date(&self, value: DateTime<Utc>) -> String {
        let formatted = match self.locale.language.as_str() {
            "pt" => format!("{:02}/{:02}/{}", value.day(), value.month(), value.year()),
            "ar" => format!("{:02}/{:02}/{}", value.day(), value.month(), value.year()),
            _ => format!("{:02}/{:02}/{}", value.month(), value.day(), value.year()),
        };
        if self.locale.language.as_str() == "ar" {
            localize_arabic_digits(&formatted)
        } else {
            formatted
        }
    }
}

fn bundle(locale: &str, source: &str) -> FluentBundle<FluentResource> {
    let locale = locale.parse().expect("bundled locale is valid");
    let resource = FluentResource::try_new(source.to_owned())
        .unwrap_or_else(|(_, errors)| panic!("invalid bundled Fluent resource: {errors:?}"));
    let mut bundle = FluentBundle::new(vec![locale]);
    bundle
        .add_resource(resource)
        .expect("bundled Fluent keys are unique");
    bundle
}

fn format_message(
    bundle: &FluentBundle<FluentResource>,
    key: MessageKey,
    args: Option<&FluentArgs<'_>>,
) -> Option<String> {
    let pattern = bundle.get_message(key.as_str())?.value()?;
    let mut errors = Vec::new();
    let value: Cow<'_, str> = bundle.format_pattern(pattern, args, &mut errors);
    errors.is_empty().then(|| value.into_owned())
}

fn localize_arabic_digits(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '0' => '٠',
            '1' => '١',
            '2' => '٢',
            '3' => '٣',
            '4' => '٤',
            '5' => '٥',
            '6' => '٦',
            '7' => '٧',
            '8' => '٨',
            '9' => '٩',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_fallback_and_arabic_direction_are_typed() {
        let localizer = Localizer::new("ar");
        assert_eq!(localizer.direction(), TextDirection::RightToLeft);
        assert!(!localizer.text(MessageKey::ActionRetry, None).is_empty());
    }

    #[test]
    fn pluralization_and_localized_digits_are_applied() {
        let english = Localizer::new("en-US");
        assert_eq!(english.count(MessageKey::DownloadCount, 1), "One download");
        let arabic = Localizer::new("ar");
        assert!(arabic.format_number(1234).contains('١'));
    }
}

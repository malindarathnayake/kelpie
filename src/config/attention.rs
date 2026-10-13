//! Fork (Q1): `[ui] stale_after` and `[ui] animations`.

use serde::{de, Deserialize, Deserializer};

/// Environment override for `[ui] animations` (`full`, `basic` or `none`).
pub const ANIMATIONS_ENV_VAR: &str = "HERDR_ANIMATIONS";

const DEFAULT_STALE_AFTER_SECS: u64 = 2 * 60 * 60;

/// Motion allowed for attention markers. `basic` and `none` both show the steady marker only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnimationsConfig {
    #[default]
    Full,
    Basic,
    None,
}

impl AnimationsConfig {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "basic" => Some(Self::Basic),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// `HERDR_ANIMATIONS` wins when it names a valid mode; anything else keeps the config.
    pub fn resolve(self, env: Option<&str>) -> Self {
        env.and_then(Self::parse).unwrap_or(self)
    }

    pub fn allows_pulse(self) -> bool {
        self == Self::Full
    }
}

/// Idle duration after which a quiet workspace is shown as stale. Accepts a number of
/// seconds or a string such as `"90m"`, `"2h"`, `"1h30m"` or `"1d"`. Zero is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleAfterConfig {
    secs: u64,
}

impl Default for StaleAfterConfig {
    fn default() -> Self {
        Self {
            secs: DEFAULT_STALE_AFTER_SECS,
        }
    }
}

impl StaleAfterConfig {
    pub fn as_millis(self) -> u64 {
        self.secs.saturating_mul(1000)
    }

    #[cfg(test)]
    pub fn secs(self) -> u64 {
        self.secs
    }

    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let mut total = 0u64;
        let mut digits = String::new();
        for ch in value.chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
                continue;
            }
            let unit = match ch.to_ascii_lowercase() {
                's' => 1,
                'm' => 60,
                'h' => 60 * 60,
                'd' => 24 * 60 * 60,
                _ => return None,
            };
            let amount = digits.parse::<u64>().ok()?;
            digits.clear();
            total = total.checked_add(amount.checked_mul(unit)?)?;
        }
        if !digits.is_empty() {
            total = total.checked_add(digits.parse::<u64>().ok()?)?;
        }
        (total > 0).then_some(Self { secs: total })
    }
}

impl<'de> Deserialize<'de> for StaleAfterConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = StaleAfterConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a positive duration such as \"2h\", \"90m\" or a number of seconds")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                if value == 0 {
                    return Err(E::invalid_value(de::Unexpected::Unsigned(value), &self));
                }
                Ok(StaleAfterConfig { secs: value })
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                let value = u64::try_from(value)
                    .map_err(|_| E::invalid_value(de::Unexpected::Signed(value), &self))?;
                self.visit_u64(value)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                StaleAfterConfig::parse(value)
                    .ok_or_else(|| E::invalid_value(de::Unexpected::Str(value), &self))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_after_parses_units_and_rejects_garbage() {
        assert_eq!(StaleAfterConfig::default().secs(), 7200);
        assert_eq!(
            StaleAfterConfig::parse("2h").map(StaleAfterConfig::secs),
            Some(7200)
        );
        assert_eq!(
            StaleAfterConfig::parse("90m").map(StaleAfterConfig::secs),
            Some(5400)
        );
        assert_eq!(
            StaleAfterConfig::parse("1h30m").map(StaleAfterConfig::secs),
            Some(5400)
        );
        assert_eq!(
            StaleAfterConfig::parse("1d").map(StaleAfterConfig::secs),
            Some(86400)
        );
        assert_eq!(
            StaleAfterConfig::parse("45").map(StaleAfterConfig::secs),
            Some(45)
        );
        for bad in ["", "0", "0h", "2x", "h", "-1h"] {
            assert_eq!(StaleAfterConfig::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn ui_section_reads_stale_after_and_animations() {
        let config: crate::config::Config =
            toml::from_str("[ui]\nstale_after = \"30m\"\nanimations = \"basic\"\n").unwrap();
        assert_eq!(config.ui.stale_after.secs(), 1800);
        assert_eq!(config.ui.animations, AnimationsConfig::Basic);
        let config: crate::config::Config = toml::from_str("[ui]\nstale_after = 600\n").unwrap();
        assert_eq!(config.ui.stale_after.secs(), 600);
        let defaults = crate::config::Config::default();
        assert_eq!(defaults.ui.stale_after.secs(), 7200);
        assert_eq!(defaults.ui.animations, AnimationsConfig::Full);
    }

    #[test]
    fn animations_env_override_wins_only_when_valid() {
        assert_eq!(
            AnimationsConfig::Full.resolve(Some("none")),
            AnimationsConfig::None
        );
        assert_eq!(
            AnimationsConfig::None.resolve(Some("FULL")),
            AnimationsConfig::Full
        );
        assert_eq!(
            AnimationsConfig::Basic.resolve(Some("sparkly")),
            AnimationsConfig::Basic
        );
        assert_eq!(
            AnimationsConfig::Basic.resolve(None),
            AnimationsConfig::Basic
        );
        assert!(AnimationsConfig::Full.allows_pulse());
        assert!(!AnimationsConfig::Basic.allows_pulse());
        assert!(!AnimationsConfig::None.allows_pulse());
    }
}

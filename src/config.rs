mod sections;
mod validate;

pub use sections::{
    MatchingConfig, RegionsConfig, ShopConfig, TemplatesConfig, TimingConfig, WindowConfig,
    ZonesConfig,
};

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const CONFIG_VERSION: u32 = 1;

/// Defaults embedded at compile time, written to disk on first run.
pub const DEFAULT_TOML: &str = include_str!("../config.toml");

#[derive(Debug, Clone)]
pub struct MissingTemplate {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct MissingZone {
    pub name: &'static str,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub shop: ShopConfig,
    #[serde(default)]
    pub timing: TimingConfig,
    #[serde(default)]
    pub matching: MatchingConfig,
    #[serde(default)]
    pub regions: RegionsConfig,
    #[serde(default)]
    pub zones: ZonesConfig,
    pub templates: TemplatesConfig,
}

impl Config {
    /// Template file existence is NOT checked here — call
    /// [`Config::ensure_templates_exist`] before running the bot.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::ConfigNotFound(path.to_path_buf()));
        }
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: Self = toml::from_str(&raw)?;
        if cfg.version != CONFIG_VERSION {
            migrate(&mut cfg)?;
        }
        validate::validate_all(&cfg)?;
        Ok(cfg)
    }

    /// `(config, created)`: `created` is true on first run so callers
    /// can surface a one-shot "we wrote the default config" message.
    pub fn load_or_init(path: &Path) -> Result<(Self, bool)> {
        if path.exists() {
            return Ok((Self::load(path)?, false));
        }

        // Validate in memory first so a broken DEFAULT_TOML release fails
        // loudly instead of leaving a poisoned file behind.
        let cfg: Self = toml::from_str(DEFAULT_TOML)?;
        validate::validate_all(&cfg)?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, DEFAULT_TOML)?;
        Ok((cfg, true))
    }

    pub fn missing_templates(&self) -> Vec<MissingTemplate> {
        validate::list_missing_templates(self)
    }

    pub fn ensure_templates_exist(&self) -> Result<()> {
        if let Some(first) = self.missing_templates().into_iter().next() {
            return Err(Error::TemplateMissing {
                name: first.name,
                path: first.path,
            });
        }
        Ok(())
    }

    pub fn missing_zones(&self) -> Vec<MissingZone> {
        validate::list_missing_zones(self)
    }

    pub fn ensure_zones_set(&self) -> Result<()> {
        if let Some(first) = self.missing_zones().into_iter().next() {
            return Err(Error::ZoneMissing { name: first.name });
        }
        Ok(())
    }

    /// Aliases the bot tries to buy this run. Single source of truth for
    /// the runner and the GUI's debug detection panel.
    pub fn enabled_targets(&self) -> Vec<&'static str> {
        use crate::detector::alias;
        let mut out = Vec::with_capacity(2);
        if self.shop.buy_mystic_medals {
            out.push(alias::MYSTIC_MEDAL);
        }
        if self.shop.buy_covenant {
            out.push(alias::COVENANT);
        }
        out
    }
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

/// No real migrations exist yet. When v2 ships, replace this body with
/// a chained dispatch:
///   while cfg.version < CONFIG_VERSION {
///       match cfg.version {
///           1 => { /* mutate to v2 */ cfg.version = 2; }
///           v => return Err(...),
///       }
///   }
fn migrate(cfg: &mut Config) -> Result<()> {
    if cfg.version < CONFIG_VERSION {
        return Err(Error::ConfigInvalid(format!(
            "no migration path from config v{} to v{CONFIG_VERSION}",
            cfg.version
        )));
    }
    if cfg.version > CONFIG_VERSION {
        return Err(Error::ConfigInvalid(format!(
            "config v{} is newer than this binary supports (v{CONFIG_VERSION}) \
             — upgrade the binary or downgrade the config",
            cfg.version
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches any commit that breaks the shipped defaults before users see it.
    #[test]
    fn default_toml_parses_and_validates() {
        let cfg: Config = toml::from_str(DEFAULT_TOML).expect("DEFAULT_TOML parse");
        validate::validate_all(&cfg).expect("DEFAULT_TOML validation");
    }

    #[test]
    fn enabled_targets_respects_shop_flags() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();

        cfg.shop.buy_mystic_medals = true;
        cfg.shop.buy_covenant = true;
        let both = cfg.enabled_targets();
        assert_eq!(both.len(), 2);
        assert!(both.contains(&crate::detector::alias::MYSTIC_MEDAL));
        assert!(both.contains(&crate::detector::alias::COVENANT));

        cfg.shop.buy_covenant = false;
        let only_mystic = cfg.enabled_targets();
        assert_eq!(only_mystic, vec![crate::detector::alias::MYSTIC_MEDAL]);

        cfg.shop.buy_mystic_medals = false;
        assert!(cfg.enabled_targets().is_empty());
    }

    #[test]
    fn migrate_errors_for_unknown_older_version() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        cfg.version = 0;
        let err = migrate(&mut cfg).expect_err("v0 must not migrate");
        assert!(format!("{err}").contains("no migration path"));
    }

    #[test]
    fn migrate_errors_for_future_version() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        cfg.version = CONFIG_VERSION + 1;
        let err = migrate(&mut cfg).expect_err("future version must not validate");
        assert!(format!("{err}").contains("newer than this binary"));
    }

    #[test]
    fn migrate_is_noop_at_current_version() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        migrate(&mut cfg).expect("current version must noop");
        assert_eq!(cfg.version, CONFIG_VERSION);
    }

    #[test]
    fn missing_zones_drops_buy_zones_when_no_buy_targets() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        cfg.zones.refresh = Some([0.1, 0.8, 0.2, 0.1]);
        cfg.zones.refresh_confirm = Some([0.4, 0.5, 0.2, 0.1]);
        cfg.zones.buy_column = None;
        cfg.zones.buy_confirm = None;

        cfg.shop.buy_mystic_medals = false;
        cfg.shop.buy_covenant = false;
        assert!(cfg.missing_zones().is_empty());

        cfg.shop.buy_mystic_medals = true;
        let names: Vec<&str> = cfg.missing_zones().iter().map(|z| z.name).collect();
        assert!(names.contains(&"buy_column"));
        assert!(names.contains(&"buy_confirm"));
    }
}

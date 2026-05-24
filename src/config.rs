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
    /// Absolute path the config was loaded from — drives where relative
    /// `templates.dir` (and other file lookups) resolve to. Not part of
    /// the on-disk schema; populated by `load_or_init`.
    #[serde(skip)]
    pub source_path: PathBuf,
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
        cfg.source_path = path.to_path_buf();
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
        let mut cfg: Self = toml::from_str(DEFAULT_TOML)?;
        cfg.source_path = path.to_path_buf();
        validate::validate_all(&cfg)?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, DEFAULT_TOML)?;
        Ok((cfg, true))
    }

    /// Absolute templates directory, resolved against the config file's
    /// parent when `templates.dir` is relative. Absolute values pass
    /// through unchanged.
    pub fn template_dir(&self) -> PathBuf {
        let dir = &self.templates.dir;
        if dir.is_absolute() {
            return dir.clone();
        }
        self.source_path
            .parent()
            .map(|p| p.join(dir))
            .unwrap_or_else(|| dir.clone())
    }

    /// Full path to a template alias, or `None` if the alias is unknown.
    /// Helper for both detection (read) and the GUI's crop saver (write).
    pub fn template_path(&self, alias: &str) -> Option<PathBuf> {
        let t = &self.templates;
        let file = match alias {
            "anchor_shop" => &t.anchor_shop,
            "mystic_medal" => &t.mystic_medal,
            "covenant" => &t.covenant,
            _ => return None,
        };
        Some(self.template_dir().join(file))
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

/// Resolves the config path with no `-c` flag, in priority order:
/// 1. A `config.toml` sitting next to the .exe (portable mode — USB
///    sticks, CI artefacts, anyone who wants the install to be
///    self-contained).
/// 2. `%APPDATA%\e7-shop-refresher\config.toml` (the Windows-canonical
///    location, where settings survive .exe replacements).
/// 3. `./config.toml` (last-resort fallback if neither of the above can
///    be resolved — only hits on broken environments).
pub fn default_config_path() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let portable = dir.join("config.toml");
        if portable.exists() {
            return portable;
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        return std::path::PathBuf::from(appdata)
            .join("e7-shop-refresher")
            .join("config.toml");
    }
    std::path::PathBuf::from("config.toml")
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
    fn template_dir_resolves_relative_path_against_config_parent() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        cfg.source_path = std::path::PathBuf::from("/some/install/dir/config.toml");
        cfg.templates.dir = std::path::PathBuf::from("templates");
        assert_eq!(
            cfg.template_dir(),
            std::path::PathBuf::from("/some/install/dir/templates")
        );
    }

    #[test]
    fn template_dir_passes_absolute_through() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        cfg.source_path = std::path::PathBuf::from("/wherever/config.toml");
        // Use a path that's absolute on both Windows (C:\...) and Unix (/...).
        let abs = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\custom\templates")
        } else {
            std::path::PathBuf::from("/custom/templates")
        };
        cfg.templates.dir = abs.clone();
        assert_eq!(cfg.template_dir(), abs);
    }

    #[test]
    fn template_path_returns_none_for_unknown_alias() {
        let cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        assert!(cfg.template_path("not_a_real_alias").is_none());
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

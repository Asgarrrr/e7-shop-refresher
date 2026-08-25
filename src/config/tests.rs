//! Tests for the config schema, its validation rules and the retired keys.
//!
//! A sibling file rather than an inline `mod tests`, matching
//! `app/session/tests.rs`. `ServerUrl`'s own tests live in `server_url.rs`.

use super::*;

use crate::domain::shop::ItemKind;
use crate::domain::shop::{Crystals, Gold};

fn parse_and_validate(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    config.validate()?;
    Ok(config)
}

/// **The regression this compatibility shim exists to prevent.**
/// `config.example.toml` shipped `buffer_size = 65575` uncommented and
/// `main::seed_config_if_missing` wrote it to every `%APPDATA%`, so under
/// `deny_unknown_fields` deleting the keys is "Invalid configuration" and an
/// app that will not start, for every player who has launched it.
#[test]
fn a_config_written_before_the_capture_keys_were_retired_still_loads() {
    let config = parse_and_validate(
        "[capture]\nbuffer_size = 65575\nfilter = \"tcp and tcp.SrcPort == 3333\"",
    )
    .expect("an upgrading player's existing config must still load");
    assert_eq!(config.capture.buffer_size, Some(65_575));
    assert_eq!(
        config.capture.filter.as_deref(),
        Some("tcp and tcp.SrcPort == 3333")
    );
}

#[test]
fn a_retired_filter_naming_another_port_is_no_longer_a_startup_failure() {
    // Used to be refused, back when a filter on another port delivered traffic
    // nothing could classify. The backend builds its own now, so refusing this
    // would lock an upgrading player out over a setting with no effect.
    let config = parse_and_validate("[capture]\nfilter = \"tcp and tcp.SrcPort == 4444\"")
        .expect("an inert setting must not stop the app from starting");
    assert!(config.capture.filter.is_some());
}

#[test]
fn the_retired_capture_keys_are_named_only_when_they_are_actually_set() {
    // This list is what the startup warning prints.
    assert_eq!(Config::default().capture.retired_keys(), None);
    assert_eq!(
        parse_and_validate("[capture]\nbuffer_size = 65575")
            .expect("still accepted")
            .capture
            .retired_keys()
            .as_deref(),
        Some("capture.buffer_size")
    );
    assert_eq!(
        parse_and_validate("[capture]\nfilter = \"tcp\"")
            .expect("still accepted")
            .capture
            .retired_keys()
            .as_deref(),
        Some("capture.filter")
    );
    assert_eq!(
        parse_and_validate("[capture]\nbuffer_size = 0\nfilter = \"tcp\"")
            .expect("still accepted")
            .capture
            .retired_keys()
            .as_deref(),
        Some("capture.buffer_size, capture.filter")
    );
}

/// **The same regression, for the `[forward]` block**, shipped uncommented in
/// `config.example.toml` too.
#[test]
fn a_config_written_before_the_forward_keys_were_retired_still_loads() {
    let config = parse_and_validate("[forward]\nserver_to_client = true\nclient_to_server = false")
        .expect("an upgrading player's existing config must still load");
    assert_eq!(config.forward.server_to_client, Some(true));
    assert_eq!(config.forward.client_to_server, Some(false));
}

#[test]
fn a_retired_forward_combination_is_no_longer_a_startup_failure() {
    // Both directions off used to be refused, describing a relay forwarding
    // nothing. Neither key reaches the pipeline now, so refusing any
    // combination would lock an upgrading player out over a dead setting.
    assert!(
        parse_and_validate("[forward]\nserver_to_client = false\nclient_to_server = false").is_ok()
    );
    // The client -> server stream is never captured, so this is inert.
    let config = parse_and_validate("[forward]\nclient_to_server = true")
        .expect("an inert setting must not stop the app from starting");
    assert_eq!(config.forward.client_to_server, Some(true));
}

#[test]
fn the_retired_forward_keys_are_named_only_when_they_are_actually_set() {
    assert_eq!(Config::default().forward.retired_keys(), None);
    assert_eq!(
        parse_and_validate("[forward]\nserver_to_client = true")
            .expect("still accepted")
            .forward
            .retired_keys()
            .as_deref(),
        Some("forward.server_to_client")
    );
    assert_eq!(
        parse_and_validate("[forward]\nclient_to_server = false")
            .expect("still accepted")
            .forward
            .retired_keys()
            .as_deref(),
        Some("forward.client_to_server")
    );
    assert_eq!(
        parse_and_validate("[forward]\nserver_to_client = true\nclient_to_server = false")
            .expect("still accepted")
            .forward
            .retired_keys()
            .as_deref(),
        Some("forward.server_to_client, forward.client_to_server")
    );
}

#[test]
fn a_misspelled_forward_key_is_still_rejected() {
    // The section is vestigial, not untyped: `deny_unknown_fields` is the only
    // way a player learns the key they meant does not exist.
    assert!(toml::from_str::<Config>("[forward]\nserver_to_clients = true").is_err());
}

#[test]
fn capture_buffer_overflow_is_still_rejected_during_deserialization() {
    // The key is ignored, not untyped: a value that cannot be a `usize` is a
    // malformed file, better reported than silently read as "unset".
    let error = parse_and_validate("[capture]\nbuffer_size = 18446744073709551616")
        .expect_err("integer overflow must fail deserialization");
    assert!(matches!(error, crate::Error::ConfigParse(_)));
}

#[test]
fn an_absent_capture_or_forward_section_leaves_every_retired_key_unset() {
    assert_eq!(Config::default().capture.buffer_size, None);
    assert_eq!(Config::default().capture.filter, None);
    assert_eq!(Config::default().forward.server_to_client, None);
    assert_eq!(Config::default().forward.client_to_server, None);
}

#[test]
fn reconnect_durations_enforce_floor_and_order() {
    let mut config = Config::default();
    assert_eq!(config.reconnect_initial(), Duration::from_millis(1_000));
    assert_eq!(config.reconnect_max(), Duration::from_millis(30_000));

    config.reconnect.initial_ms = 1;
    config.reconnect.max_ms = 10;
    assert_eq!(config.reconnect_initial(), RECONNECT_FLOOR);
    assert_eq!(config.reconnect_max(), RECONNECT_FLOOR);

    config.reconnect.initial_ms = 2_000;
    config.reconnect.max_ms = 1_000;
    assert_eq!(config.reconnect_initial(), Duration::from_millis(2_000));
    assert_eq!(config.reconnect_max(), Duration::from_millis(2_000));
}

#[test]
fn misspelled_kind_value_is_rejected() {
    // `ItemKind`'s `serde(other)` folds a typo into `Unknown`, which matches
    // nothing while `is_unrestricted` counts it as a criterion — the loop then
    // burns crystals forever. `Filter::kinds` holds `HuntKind`, no catch-all,
    // so the refusal is serde's and names the three legal values.
    let error = parse_and_validate("[filter]\nkinds = [\"equipement\"]")
        .expect_err("a misspelled kind must not silently match nothing");
    assert!(matches!(error, crate::Error::ConfigParse(_)), "{error:?}");
    let message = error.report();
    for expected in ["equipement", "equipment", "hero", "token"] {
        assert!(message.contains(expected), "{message}");
    }
    // And the value the shipped checkbox wrote goes the same way, instead of
    // parsing into a criterion nothing can ever satisfy.
    assert!(matches!(
        parse_and_validate("[filter]\nkinds = [\"unknown\"]"),
        Err(crate::Error::ConfigParse(_))
    ));
}

#[test]
fn full_filter_and_limits_sections_parse() {
    let config: Config = toml::from_str(
        r#"
            [filter]
            kinds = ["equipment", "hero"]
            sets = ["set_speed", "set_counter"]
            min_substats = 3
            max_price = 300000
            include_sold_out = true

            [[filter.required_substats]]
            name = "speed"
            min = 8.0

            [[filter.required_substats]]
            name = "cri"

            [limits]
            max_refreshes = 100
            max_spend = 300
            max_matches = 5
            max_duration_ms = 3600000
            "#,
    )
    .expect("config should parse");

    assert_eq!(
        config.filter.kinds,
        vec![ItemKind::Equipment, ItemKind::Hero]
    );
    assert!(config.filter.include_sold_out);
    // The flat gear keys fold into one rule — the shape they always described.
    let rule = config.filter.only_rule();
    assert_eq!(rule.sets, vec!["set_speed", "set_counter"]);
    assert_eq!(rule.min_substats, Some(3));
    assert_eq!(rule.max_price, Some(Gold::new(300_000)));
    assert_eq!(rule.required_substats.len(), 2);
    assert_eq!(rule.required_substats[0].name, "speed");
    assert_eq!(rule.required_substats[0].min, Some(8.0));
    assert_eq!(rule.required_substats[1].name, "cri");
    assert_eq!(rule.required_substats[1].min, None);

    assert_eq!(config.limits.max_refreshes, Some(100));
    assert_eq!(config.limits.max_spend, Some(Crystals::new(300)));
    assert_eq!(config.limits.max_matches, Some(5));
    assert_eq!(config.limits.max_duration_ms, Some(3_600_000));
}

#[test]
fn a_zero_game_port_is_rejected_by_the_type() {
    // `NonZeroU16`, so the refusal is serde's — and the BPF filter string and
    // `parse_segment`, which read it with no `Config` in scope, can no longer
    // be handed a zero that matches nothing and calls every packet client-sent.
    let error = parse_and_validate("game_port = 0").expect_err("port 0 is not a port");
    assert!(matches!(error, crate::Error::ConfigParse(_)), "{error:?}");
    assert!(error.report().contains("game_port"), "{}", error.report());
    // The neighbouring values still parse, so the bound is exactly `> 0`.
    assert_eq!(
        parse_and_validate("game_port = 1")
            .expect("port 1 is a port")
            .game_port
            .get(),
        1
    );
    assert_eq!(Config::default().game_port, DEFAULT_GAME_PORT);
}

#[test]
fn missing_filter_and_limits_sections_default() {
    let config: Config = toml::from_str("game_port = 3333").expect("config should parse");
    assert!(config.filter.kinds.is_empty());
    assert!(config.filter.gear.is_empty());
    assert_eq!(config.limits.max_refreshes, None);
    assert_eq!(config.limits.max_spend, None);
}

#[test]
fn partial_sections_leave_other_fields_default() {
    let config: Config = toml::from_str(
        r#"
            [filter]
            min_substats = 4

            [limits]
            max_spend = 50
            "#,
    )
    .expect("config should parse");
    assert_eq!(config.filter.only_rule().min_substats, Some(4));
    assert!(config.filter.kinds.is_empty());
    assert_eq!(config.limits.max_spend, Some(Crystals::new(50)));
    assert_eq!(config.limits.max_refreshes, None);
}

#[test]
fn actuator_section_parses_and_defaults_off() {
    let config: Config = toml::from_str("[actuator]\ndry_run = true").expect("config should parse");
    assert!(config.actuator.dry_run);
    // Absent section (and absent key) default to a live actuator.
    let config: Config = toml::from_str("[actuator]").expect("config should parse");
    assert!(!config.actuator.dry_run);
    assert!(!Config::default().actuator.dry_run);
}

#[test]
fn misspelled_actuator_key_is_rejected() {
    // A silently ignored `dry_run` typo would send real clicks.
    assert!(toml::from_str::<Config>("[actuator]\ndryrun = true").is_err());
}

#[test]
fn actuator_backend_parses_and_defaults_to_message() {
    let config: Config =
        toml::from_str("[actuator]\nbackend = \"input\"").expect("config should parse");
    assert_eq!(config.actuator.backend, ActuatorBackend::Input);
    // Absent key: the message backend, so the player keeps the mouse.
    let config: Config = toml::from_str("[actuator]").expect("config should parse");
    assert_eq!(config.actuator.backend, ActuatorBackend::Message);
    assert_eq!(Config::default().actuator.backend, ActuatorBackend::Message);
}

#[test]
fn actuator_timings_parse_and_default_to_zero() {
    let config: Config = toml::from_str(
        r#"
            [actuator.timings]
            refreshed = { min_ms = 200, max_ms = 800 }
            between_buys = { min_ms = 100, max_ms = 500 }
            "#,
    )
    .expect("config should parse");
    assert_eq!(config.actuator.timings.refreshed.min_ms(), 200);
    assert_eq!(config.actuator.timings.refreshed.max_ms(), 800);
    assert_eq!(config.actuator.timings.between_buys.max_ms(), 500);
    // Unset ranges stay at the calibrated baseline (0..=0 extra).
    assert_eq!(config.actuator.timings.shop_opened.max_ms(), 0);
    assert_eq!(Config::default().actuator.timings.refreshed.max_ms(), 0);
}

#[test]
fn reversed_timing_range_is_rejected_and_names_both_values() {
    // Accepted, `{ min_ms = 800, max_ms = 200 }` would silently reread as a
    // fixed 800 ms delay. `DelayRange`'s `try_from` refuses it, so it is a
    // *parse* error with the line quoted. Not through `Config::load`, which
    // deliberately no longer dies on this (see `drop_unreadable_timing_ranges`)
    // — what this pins is that the deserializer refuses the value either way.
    let error =
        parse_and_validate("[actuator.timings]\nrefreshed = { min_ms = 800, max_ms = 200 }")
            .expect_err("a reversed range must not be silently reinterpreted");
    // `report()`, not `to_string()`: the chain walk is what gets printed.
    let message = error.report();
    assert!(matches!(error, crate::Error::ConfigParse(_)), "{error:?}");
    assert!(message.contains("refreshed"), "{message}");
    assert!(
        message.contains("800") && message.contains("200"),
        "{message}"
    );
    // And the reason survives the trip through serde, unchanged.
    assert!(message.contains("fixed 800 ms delay"), "{message}");
}

#[test]
fn an_oversized_timing_range_is_rejected() {
    // Ten minutes between two clicks is indistinguishable from a hang.
    let error =
        parse_and_validate("[actuator.timings]\nrefreshed = { min_ms = 0, max_ms = 600000 }")
            .expect_err("a ten-minute extra wait must be refused");
    assert!(matches!(error, crate::Error::ConfigParse(_)), "{error:?}");
    assert!(error.report().contains("refreshed"), "{}", error.report());
    assert!(error.report().contains("ceiling"), "{}", error.report());

    // The overflow case: four bare additions in the timing editor sum this
    // with a baseline. Rejecting it at the type is the guard at the root.
    let error = parse_and_validate(
        "[actuator.timings]\nshop_opened = { min_ms = 0, max_ms = 18446744073709551615 }",
    )
    .expect_err("a u64::MAX extra wait must be refused");
    assert!(matches!(error, crate::Error::ConfigParse(_)), "{error:?}");
    assert!(error.report().contains("shop_opened"), "{}", error.report());
}

#[test]
fn every_timing_range_is_checked_not_just_the_first() {
    // `DelayRange`'s `try_from` cannot miss a field by construction, but the
    // eight keys must still each *be* a `DelayRange`.
    for name in Timings::default().named_ranges().map(|(name, _)| name) {
        let text = format!("[actuator.timings]\n{name} = {{ min_ms = 9, max_ms = 1 }}");
        match parse_and_validate(&text) {
            Ok(_) => panic!("actuator.timings.{name} is not validated"),
            Err(error) => assert!(error.report().contains(name), "{name}: {}", error.report()),
        }
    }
}

#[test]
fn a_timing_range_at_the_ceiling_is_accepted() {
    // The ceiling stops a frozen loop; it is not meant to narrow the knob.
    let ceiling = crate::actuator::plan::MAX_TIMING_MS;
    let config = parse_and_validate(&format!(
        "[actuator.timings]\nrefreshed = {{ min_ms = 0, max_ms = {ceiling} }}"
    ))
    .expect("the ceiling itself is a legal setting");
    assert_eq!(config.actuator.timings.refreshed.max_ms(), ceiling);
    // A point range (min == max) is a fixed extra, not a reversed range.
    assert!(
        parse_and_validate("[actuator.timings]\nbuy_modal = { min_ms = 500, max_ms = 500 }")
            .is_ok()
    );
    // And the all-zero default still validates.
    assert!(Config::default().validate().is_ok());
}

#[test]
fn every_timing_preset_survives_validation() {
    // The Setup tab writes these verbatim through persist::save; a preset
    // the loader then refuses would lock the player out on next launch.
    for preset in crate::actuator::plan::TimingPreset::ALL {
        let config = Config {
            actuator: ActuatorConfig {
                timings: preset.timings(),
                ..ActuatorConfig::default()
            },
            ..Config::default()
        };
        assert!(
            config.validate().is_ok(),
            "preset {} must round-trip through the config",
            preset.label()
        );
    }
}

/// **The regression this salvage pass exists to prevent.** `DelayRange::draw`
/// read a reversed pair as a point at `min_ms`, so this file was a *working*
/// config until the type started refusing it — at which point `Config::load`
/// failed, `main` turned that into `fatal()`, and the only cure was
/// hand-editing a `%APPDATA%` file the Setup tab otherwise owns.
#[test]
fn a_players_reversed_timing_range_no_longer_bricks_the_app() {
    let dir = TempDir::new("unreadable-timings");
    std::fs::create_dir_all(dir.path()).expect("fixture dir");
    let path = dir.join("config.toml");
    // Both shapes: the transposed pair, and the over-ceiling shorthand whose
    // omitted `min_ms` must read as the 0 `serde` would have used.
    let text = "\
# hand-written
game_port = 3333

[filter]
names = [\"ticketrare_name\"]

[actuator.timings]
shop_opened = { min_ms = 100, max_ms = 900 }
refreshed = { min_ms = 800, max_ms = 200 }
between_buys = { max_ms = 120000 }
";
    std::fs::write(&path, text).expect("seed a pre-refusal config");

    let (config, dropped) =
        Config::load_reporting(&path).expect("an existing config must still open the app");
    // The half the log file cannot deliver in a windowed build. One line for
    // both keys, because the advice is the same for either.
    let lines = dropped.journal_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("actuator.timings.refreshed"), "{lines:?}");
    assert!(
        lines[0].contains("actuator.timings.between_buys"),
        "{lines:?}"
    );
    assert!(lines[0].contains("they are not in force"), "{lines:?}");
    // Dropped to the default, which is exactly what an absent key means.
    assert_eq!(config.actuator.timings.refreshed, DelayRange::default());
    assert_eq!(config.actuator.timings.between_buys, DelayRange::default());
    // The pass drops the key it cannot read, never the section.
    assert_eq!(config.actuator.timings.shop_opened.min_ms(), 100);
    assert_eq!(config.actuator.timings.shop_opened.max_ms(), 900);
    assert_eq!(config.game_port.get(), 3333);
    assert_eq!(config.filter.names, vec!["ticketrare_name".to_owned()]);
    config.validate().expect("and the result must validate");

    // The file is left alone, comment and typo included: swapping the pair
    // would be writing a guess at which number was wrong into their file.
    assert_eq!(
        std::fs::read_to_string(&path).expect("still readable"),
        text,
        "the loader must not rewrite the player's config"
    );

    // The same fixture minus the two bad ranges — every launch after the first
    // — must put nothing in the journal at all.
    std::fs::write(
        &path,
        "game_port = 3333\n\n[filter]\nnames = [\"ticketrare_name\"]\n\n[actuator.timings]\nshop_opened = { min_ms = 100, max_ms = 900 }\n",
    )
    .expect("seed a clean config");
    let (_, none) = Config::load_reporting(&path).expect("a clean config loads");
    assert!(
        none.journal_lines().is_empty(),
        "a clean load must not put a line in the journal"
    );
}

/// **The bug this test pins.** `written_range` used to resolve an omitted
/// `max_ms` to `0`, not to the resolved `min_ms` the way `RawDelayRange` does.
/// A floor-only range like `shop_opened = { min_ms = 200 }` is the fixed delay
/// `200..=200` to `serde` — legal — but the salvage pass judged it as
/// `(200, 0)`, saw `DelayRangeError::Reversed`, and deleted a range the player
/// wrote correctly. `refreshed` is the genuinely invalid pair that must be
/// present for the salvage pass to run at all.
#[test]
fn a_floor_only_range_survives_the_salvage_of_a_neighbouring_bad_one() {
    let dir = TempDir::new("floor-only-timings");
    std::fs::create_dir_all(dir.path()).expect("fixture dir");
    let path = dir.join("config.toml");
    let text = "\
# hand-written
game_port = 3333

[filter]
names = [\"ticketrare_name\"]

[actuator.timings]
shop_opened = { min_ms = 200 }
refreshed = { min_ms = 800, max_ms = 200 }
";
    std::fs::write(&path, text).expect("seed a pre-refusal config");

    let (config, dropped) =
        Config::load_reporting(&path).expect("an existing config must still open the app");
    let lines = dropped.journal_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("actuator.timings.refreshed"), "{lines:?}");
    assert!(
        !lines[0].contains("actuator.timings.shop_opened"),
        "a floor-only range is legal and must not be reported as dropped: {lines:?}"
    );
    // The fixed-delay semantics `RawDelayRange` defines: an omitted `max_ms`
    // resolves to the resolved `min_ms`, not to 0.
    assert_eq!(config.actuator.timings.shop_opened.min_ms(), 200);
    assert_eq!(config.actuator.timings.shop_opened.max_ms(), 200);
    config.validate().expect("and the result must validate");

    // The file is left alone: the salvage pass only removes candidates it
    // cannot read, never rewrites the ones it can.
    assert_eq!(
        std::fs::read_to_string(&path).expect("still readable"),
        text,
        "the loader must not rewrite the player's config"
    );
}

/// **Guards the invariant the floor-only bug broke.** `written_range` and
/// `RawDelayRange`'s `TryFrom` (`actuator::plan::timings`) are two
/// implementations of one rule, in two files, and they drifted once already —
/// this pins them together directly instead of only covering the one
/// spelling that surfaced the drift, so the next disagreement between the two
/// fails here rather than shipping.
///
/// Lives in `config::tests`, not `plan::timings`, because `written_range` is
/// private to `config` and this file reaches it through `use super::*`;
/// `plan::timings` has no way to see it at all.
#[test]
fn written_range_agrees_with_what_serde_actually_builds() {
    for spelling in [
        "{ min_ms = 200 }",
        "{ max_ms = 900 }",
        "{ min_ms = 100, max_ms = 900 }",
        "{}",
    ] {
        let text = format!("[actuator.timings]\nshop_opened = {spelling}\n");

        let doc: DocumentMut = text.parse().expect("valid toml");
        let item = doc
            .as_table()
            .get("actuator")
            .and_then(Item::as_table_like)
            .and_then(|table| table.get("timings"))
            .and_then(Item::as_table_like)
            .and_then(|table| table.get("shop_opened"))
            .expect("the fixture always writes shop_opened");
        let from_written_range = written_range(item).expect("every spelling here is readable");

        let config: Config = toml::from_str(&text).expect("serde must accept the same text");
        let from_serde = (
            config.actuator.timings.shop_opened.min_ms(),
            config.actuator.timings.shop_opened.max_ms(),
        );

        assert_eq!(
            from_written_range, from_serde,
            "{spelling:?}: written_range and serde disagree on the pair"
        );
    }
}

#[test]
fn an_unreadable_range_is_dropped_whichever_way_the_table_is_spelled() {
    // All four spellings are the same document to `toml`, so a player bricked
    // by any of them must be unbricked by all of them.
    for text in [
        "[actuator.timings]\nrefreshed = { min_ms = 800, max_ms = 200 }\n",
        "actuator.timings.refreshed = { min_ms = 800, max_ms = 200 }\n",
        "actuator = { timings = { refreshed = { min_ms = 800, max_ms = 200 } } }\n",
        "[actuator.timings.refreshed]\nmin_ms = 800\nmax_ms = 200\n",
    ] {
        let (config, dropped) =
            parse(text).unwrap_or_else(|err| panic!("{text:?}: {}", err.report()));
        assert_eq!(
            config.actuator.timings.refreshed,
            DelayRange::default(),
            "{text:?}"
        );
        // A salvage the GUI cannot name is a setting silently not in force.
        assert_eq!(
            dropped.journal_lines().len(),
            1,
            "{text:?}: the drop must be reportable, not only logged"
        );
        assert!(
            dropped.journal_lines()[0].contains("actuator.timings.refreshed"),
            "{text:?}: {:?}",
            dropped.journal_lines()
        );
    }
}

#[test]
fn the_salvage_pass_rules_on_nothing_but_the_ranges_it_can_read() {
    // Anything the pass does not understand must survive to be refused
    // properly, or a real mistake loads as "range dropped, carry on".
    for text in [
        // A misspelled key beside the reversed one: still `deny_unknown_fields`.
        "[actuator.timings]\nrefreshed = { min_ms = 800, max_ms = 200 }\nrefesh = { max_ms = 5 }\n",
        // Not two integers: a different complaint, reported far better by serde.
        "[actuator.timings]\nrefreshed = { min_ms = \"800\", max_ms = 200 }\n",
        "[actuator.timings]\nrefreshed = { min_ms = -800, max_ms = 200 }\n",
        // Nothing to do with timings at all: the original refusal, unchanged.
        "game_port = 0\n",
        "[filter]\nkinds = [\"equipement\"]\n",
    ] {
        assert!(
            parse(text).is_err(),
            "{text:?} must still be refused outright"
        );
    }
    // The misspelling is what the player is told about, not the range already
    // dropped: the stale error points at a line that is no longer there.
    let error = parse(
        "[actuator.timings]\nrefreshed = { min_ms = 800, max_ms = 200 }\nrefesh = { max_ms = 5 }\n",
    )
    .expect_err("an unknown key is still an unknown key");
    assert!(error.report().contains("refesh"), "{}", error.report());
}

#[test]
fn a_valid_file_never_reaches_the_salvage_pass() {
    // A well-formed config must pay no second parse and no rewrite.
    assert!(
        drop_unreadable_timing_ranges(
            "[actuator.timings]\nrefreshed = { min_ms = 200, max_ms = 800 }\n\
             buy_modal = { min_ms = 500, max_ms = 500 }\n"
        )
        .is_none()
    );
    // Including the two boundary values, which are legal and must not be
    // mistaken for the thing this pass removes.
    let ceiling = crate::actuator::plan::MAX_TIMING_MS;
    assert!(
        drop_unreadable_timing_ranges(&format!(
            "[actuator.timings]\nrefreshed = {{ min_ms = {ceiling}, max_ms = {ceiling} }}\n"
        ))
        .is_none()
    );
    // No `[actuator]` section — the common case — declines at the first hop.
    assert!(drop_unreadable_timing_ranges("game_port = 3333\n").is_none());
    assert!(drop_unreadable_timing_ranges("[actuator]\ndry_run = true\n").is_none());
}

#[test]
fn misspelled_timings_key_is_rejected() {
    // A silently ignored typo would leave the loop at the baseline while
    // the player thinks they slowed it down.
    assert!(toml::from_str::<Config>("[actuator.timings]\nrefesh = { min_ms = 500 }").is_err());
    // A typo inside a range is caught too (deny_unknown_fields on the range).
    assert!(toml::from_str::<Config>("[actuator.timings.refreshed]\nminms = 500").is_err());
}

#[test]
fn unknown_actuator_backend_is_rejected() {
    // A silently defaulted typo would steal the mouse the player kept.
    assert!(toml::from_str::<Config>("[actuator]\nbackend = \"postmessage\"").is_err());
}

#[test]
fn misspelled_limit_key_is_rejected() {
    // A silently ignored typo would mean a limit that never triggers.
    assert!(toml::from_str::<Config>("[limits]\nmax_refresh = 10").is_err());
    assert!(toml::from_str::<Config>("[filter]\nmax_prices = 10").is_err());
}

#[test]
fn required_substat_without_name_is_rejected() {
    let result = toml::from_str::<Config>(
        r#"
            [[filter.required_substats]]
            min = 8.0
            "#,
    );
    assert!(result.is_err());
}

#[test]
fn bundled_example_config_parses_validates_and_is_restrictive() {
    // `main::seed_config_if_missing` writes this text to every player's
    // %APPDATA% and nothing else deserializes it, so without this it rots
    // silently while CI stays green and the shipped exe greets a new player
    // with "Invalid configuration".
    let text = include_str!("../../config.example.toml");
    let config: Config = toml::from_str(text).expect("the bundled example must parse");
    config
        .validate()
        .expect("the bundled example must validate");
    // The relay refuses to arm on an unrestricted filter (`app::run`), so a
    // criterion-less example would seed a file that cannot start a hunt.
    assert!(
        !config.filter.is_unrestricted(),
        "the example must carry a hunt criterion"
    );
    // Uncommenting either retired line here would hand every *new* player a
    // first launch that warns about the example the app just seeded.
    assert_eq!(config.capture.retired_keys(), None);
    assert_eq!(config.forward.retired_keys(), None);

    // The same on disk: offered to the stripper, the example must come back
    // untouched — an empty table is a commented section, not a leftover.
    let dir = TempDir::new("example-strip");
    let path = dir.join("config.toml");
    // Through the real seeder, not a hand-rolled `fs::write`, so changing it
    // cannot leave this passing against a stale copy of what it used to do.
    crate::seed_config_if_missing(&path);
    assert_eq!(
        std::fs::read_to_string(&path).expect("the seeder created it"),
        text,
        "the seeder must write the bundled example verbatim"
    );
    assert_eq!(
        persist::strip_retired_keys(&path).expect("must not fail"),
        None,
        "a fresh install must have nothing to strip"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("still readable"),
        text,
        "the seeded example must be left byte-identical"
    );
}

/// Scratch directory removed on drop, including when an assertion panics. The
/// name mixes the pid with a process-local counter so two test binaries, or two
/// parallel tests in one binary, cannot collide.
struct TempDir(std::path::PathBuf);

impl TempDir {
    /// The directory is deliberately **not** created: the save test needs it
    /// absent to prove `persist::save` builds it.
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "arkyve-refresh-shop-test-{tag}-{}-{unique}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn save_then_load_round_trips_the_edited_sections_through_disk() {
    use crate::actuator::plan::DelayRange;
    use crate::config::persist::{self, Section};

    let dir = TempDir::new("save-load");
    assert!(!dir.path().exists(), "the fixture must start absent");
    let path = dir.join("config.toml");

    let filter = Filter {
        names: vec!["ticketrare_name".to_owned()],
        ..Filter::default()
    };
    let limits = Limits {
        max_refreshes: Some(10),
        max_spend: Some(Crystals::new(30)),
        ..Limits::default()
    };
    let timings = Timings {
        refreshed: DelayRange::try_new(200, 800).expect("a valid fixture range"),
        ..Timings::default()
    };

    persist::save(
        &path,
        &[
            Section::Filter(filter.clone()),
            Section::Limits(limits),
            Section::Timings(timings),
        ],
    )
    .expect("save must create the missing directory and the file");

    assert!(path.exists(), "create_dir_all covered the missing parent");
    assert!(
        !path.with_extension("toml.tmp").exists(),
        "the atomic-write temp must not survive a successful save"
    );

    let config = Config::load(&path).expect("the file we just wrote must load");
    config.validate().expect("and must validate");
    assert_eq!(config.filter, filter);
    assert_eq!(config.limits, limits);
    assert_eq!(config.actuator.timings, timings);
}

#[test]
fn stripping_a_players_config_clears_the_retired_warning_for_good() {
    // End to end, with everything else the player wrote intact.
    let dir = TempDir::new("strip-retired");
    std::fs::create_dir_all(dir.path()).expect("fixture dir");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "# hand-written\ngame_port = 3333\n\n[forward]\nserver_to_client = true\n\n\
             [capture]\nbuffer_size = 65575\n\n[filter]\nnames = [\"ticketrare_name\"]\n",
    )
    .expect("seed a pre-retirement config");

    let before = Config::load(&path).expect("an upgrading player's config still loads");
    assert!(before.capture.retired_keys().is_some());
    assert!(before.forward.retired_keys().is_some());

    let removed = persist::strip_retired_keys(&path)
        .expect("the rewrite must succeed on a writable file")
        .expect("both keys were set, so it must have rewritten");
    assert_eq!(removed, "capture.buffer_size, forward.server_to_client");
    assert!(
        !path.with_extension("toml.tmp").exists(),
        "the atomic-write temp must not survive a successful strip"
    );

    let after = Config::load(&path).expect("the stripped file must still load");
    assert_eq!(after.capture.retired_keys(), None, "no warning next launch");
    assert_eq!(after.forward.retired_keys(), None, "no warning next launch");
    assert_eq!(after.game_port.get(), 3333);
    assert_eq!(after.filter, before.filter, "the hunt is untouched");
    let text = std::fs::read_to_string(&path).expect("readable");
    assert!(text.contains("# hand-written"), "comments survive: {text}");

    assert_eq!(
        persist::strip_retired_keys(&path).expect("must not fail"),
        None
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("readable"),
        text,
        "a second pass must leave the file byte-identical"
    );
}

#[test]
fn a_failed_strip_leaves_the_retired_keys_in_place_to_warn_about() {
    // The strip is not allowed to be fatal, and when it fails the keys really
    // are still on disk — which is why `main` still warns on this branch.
    let dir = TempDir::new("failed-strip");
    std::fs::create_dir_all(dir.path()).expect("fixture dir");
    let path = dir.join("config.toml");
    let original = "[capture]\nbuffer_size = 65575\n";
    std::fs::write(&path, original).expect("seed the original");
    std::fs::create_dir(path.with_extension("toml.tmp")).expect("squat the temp path");

    let error = persist::strip_retired_keys(&path)
        .expect_err("the temp write cannot succeed onto a directory");
    assert!(
        matches!(error, crate::Error::ConfigWrite { .. }),
        "expected a path-carrying write error, got {error:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("original still readable"),
        original,
        "a failed strip must not touch the player's file"
    );
    assert!(
        Config::load(&path)
            .expect("and the app must still start")
            .capture
            .retired_keys()
            .is_some()
    );
}

#[test]
fn stripping_a_missing_config_is_not_an_error() {
    // `seed_config_if_missing` can fail on an unwritable %APPDATA%, so a
    // missing file must be "nothing to do", not a startup error report.
    let dir = TempDir::new("strip-missing");
    assert_eq!(
        persist::strip_retired_keys(dir.join("config.toml"))
            .expect("a missing file is not an error"),
        None
    );
}

#[test]
fn load_on_a_missing_file_yields_the_defaults() {
    // The `NotFound` branch is what every machine without a config.toml takes
    // at startup; turning it into an error would be invisible in CI.
    let dir = TempDir::new("missing");
    let path = dir.join("config.toml");
    assert!(!path.exists());

    let config = Config::load(&path).expect("a missing file is not an error");
    assert_eq!(config.game_port, DEFAULT_GAME_PORT);
    assert_eq!(config.server_url, Config::default().server_url);
    assert!(config.filter.is_unrestricted(), "defaults set no criterion");
}

#[test]
fn a_failed_save_leaves_the_original_config_intact() {
    // Squatting the temp path with a directory fails the sibling write. A
    // "simplification" to `fs::write(path, ..)` would succeed here instead,
    // having already truncated the player's file.
    use crate::config::persist::{self, Section};

    let dir = TempDir::new("failed-save");
    std::fs::create_dir_all(dir.path()).expect("fixture dir");
    let path = dir.join("config.toml");
    let original = "# hand-written\ngame_port = 3333\n";
    std::fs::write(&path, original).expect("seed the original");
    std::fs::create_dir(path.with_extension("toml.tmp")).expect("squat the temp path");

    let error = persist::save(
        &path,
        &[Section::Limits(Limits {
            max_refreshes: Some(7),
            ..Limits::default()
        })],
    )
    .expect_err("the temp write cannot succeed onto a directory");

    assert!(
        matches!(error, crate::Error::ConfigWrite { .. }),
        "expected a path-carrying write error, got {error:?}"
    );
    assert!(
        error.to_string().contains("config.toml"),
        "the message must name the file: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("original still readable"),
        original,
        "a failed save must not touch the player's file"
    );
}

#[test]
fn an_unreadable_config_reports_the_path_not_a_bare_os_error() {
    // A directory where the file should be: not `NotFound`, so it takes the
    // read-error branch. Without a path the player sees only "i/o: Access is
    // denied. (os error 5)" about a %APPDATA% file they never navigate to.
    let dir = TempDir::new("unreadable");
    std::fs::create_dir_all(dir.join("config.toml")).expect("fixture dir");

    let error = Config::load(dir.join("config.toml")).expect_err("a directory cannot be read");
    assert!(
        matches!(error, crate::Error::ConfigRead { .. }),
        "expected a path-carrying read error, got {error:?}"
    );
    assert!(
        error.to_string().contains("config.toml"),
        "the message must name the file: {error}"
    );
}

#[test]
fn a_non_finite_substat_threshold_is_rejected() {
    // `nan`/`inf` are legal TOML 1.0 float literals. Accepted, `value >= min`
    // is false for every value, so the filter matches nothing while
    // `is_unrestricted()` counts it as a criterion — the loop arms and
    // refreshes forever, debiting crystals. `nan` also breaks `Filter`'s
    // derived `PartialEq`, so the Setup tab's dirty check never clears.
    for literal in ["nan", "inf", "-inf", "-nan"] {
        let text = format!("[[filter.required_substats]]\nname = \"speed\"\nmin = {literal}\n");
        let error = parse_and_validate(&text)
            .expect_err("a threshold no value can satisfy must be refused");
        assert!(matches!(error, crate::Error::Config(_)), "{literal}");
        let message = error.to_string();
        assert!(
            message.contains("speed"),
            "must name the requirement: {message}"
        );
    }
}

#[test]
fn a_finite_substat_threshold_including_zero_and_negative_is_accepted() {
    // A zero or negative floor is meaningful (some substats are stored as
    // deltas) and must stay reachable.
    let config = parse_and_validate(
        "[[filter.required_substats]]\nname = \"speed\"\nmin = 0.0\n\n\
             [[filter.required_substats]]\nname = \"cri\"\nmin = -1.5\n\n\
             [[filter.required_substats]]\nname = \"atk\"\n",
    )
    .expect("finite thresholds are legal");
    assert_eq!(config.filter.only_rule().required_substats.len(), 3);
    assert_eq!(config.filter.only_rule().required_substats[2].min, None);
}

#[test]
fn a_configs_debug_redacts_the_server_url() {
    // `Config` is exactly the kind of value that ends up in a startup line.
    // `Debug` is a plain derive, safe only because the field is a `ServerUrl`.
    let rendered = format!(
        "{:?}",
        Config {
            server_url: ServerUrl::parse("wss://token:secret@host:8443/p?k=v")
                .expect("wss is accepted whatever the authority carries"),
            ..Config::default()
        }
    );
    assert!(!rendered.contains("secret"), "{rendered}");
    assert!(rendered.contains("wss://host:8443"), "{rendered}");
}

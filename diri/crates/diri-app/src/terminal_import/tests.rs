use std::path::{Path, PathBuf};

use diri_term::theme::TermTheme;
use diri_term::theme_file::{hex_string, parse_color};

use super::*;

fn fixture_home(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/terminal-setup")
        .join(name)
}

fn roots(name: &str) -> Roots {
    let mut roots = Roots::in_home(&fixture_home(name));
    // Never the bundled Ghostty.app themes of whoever runs the tests.
    roots.ghostty_resources.clear();
    roots
}

const INSTALLED: &[&str] = &["JetBrains Mono", "Fira Code", "Menlo", "SF Mono", "Iosevka Term"];

fn installed(name: &str) -> Option<String> {
    match_font_family(name, INSTALLED.iter().copied())
}

fn env() -> Environment<'static> {
    Environment {
        resolve_font: &installed,
    }
}

fn hex(color: Option<Rgba>) -> Option<String> {
    color.map(hex_string)
}

fn spec(setup: &Setup) -> &ThemeSpec {
    setup
        .theme
        .as_ref()
        .and_then(|theme| theme.spec.as_ref())
        .expect("theme colors")
}

fn skipped(setup: &Setup) -> Vec<SkipReason> {
    setup.skipped.iter().map(|skip| skip.reason).collect()
}

/// Nothing a config holds beyond the settings Diri reads may surface.
fn assert_no_secret(setup: &Setup, secret: &str) {
    let plan = plan(Source::Ghostty, setup, &Prefs::default(), &env());
    let text = format!("{setup:?}{plan:?}");
    assert!(!text.contains(secret), "{secret} leaked: {text}");
}

#[test]
fn ghostty_reads_includes_named_themes_and_only_what_was_set() {
    let setup = ghostty::read(&roots("ghostty")).expect("ghostty config");
    // The first family after the last reset is the face; fallbacks are not.
    assert_eq!(setup.font_family.as_deref(), Some("JetBrains Mono"));
    // `config-file` includes load after the file and override it.
    assert_eq!(setup.font_size, Some(15.0));
    assert_eq!(setup.line_height, Some(LineHeight::Scale(1.2)));
    assert_eq!(setup.cursor_shape, Some(CursorShapePref::Bar));
    assert_eq!(setup.cursor_blink, Some(CursorBlinkPref::Off));
    assert_eq!(setup.copy_on_select, Some(true));
    let theme = setup.theme.as_ref().unwrap();
    // `light:A,dark:B` takes the dark theme; inline colors beat the theme's.
    assert_eq!(theme.name, "Night Owl Custom");
    let spec = spec(&setup);
    assert_eq!(hex(Some(spec.background)).as_deref(), Some("#011627"));
    assert_eq!(hex(Some(spec.foreground)).as_deref(), Some("#d6deeb"));
    assert_eq!(hex(Some(spec.ansi[4])).as_deref(), Some("#82aaff"));
    assert_eq!(hex(spec.selection).as_deref(), Some("#1d3b53"));
    assert_eq!(
        skipped(&setup),
        [
            SkipReason::OptionKey,
            SkipReason::Padding,
            SkipReason::Scrollback,
            SkipReason::KeyBindings,
        ]
    );
    assert!(setup.unknown.contains(&"term".to_owned()));
    assert!(setup.unknown.contains(&"custom-shader".to_owned()));
    // config, the include, and the theme; the optional missing include is
    // not an error.
    assert_eq!(setup.files.len(), 3, "{:?}", setup.files);
    assert_no_secret(&setup, "secret-shader");
}

#[test]
fn kitty_reads_includes_and_kitten_theme_names() {
    let setup = kitty::read(&roots("kitty")).expect("kitty config");
    assert_eq!(setup.font_family.as_deref(), Some("Fira Code"));
    assert_eq!(setup.font_size, Some(12.5));
    assert_eq!(setup.line_height, Some(LineHeight::Scale(1.1)));
    assert_eq!(setup.cursor_shape, Some(CursorShapePref::Bar));
    assert_eq!(setup.cursor_blink, Some(CursorBlinkPref::Off));
    assert_eq!(setup.copy_on_select, Some(true));
    assert_eq!(setup.theme.as_ref().unwrap().name, "Tokyo Night Test");
    let spec = spec(&setup);
    assert_eq!(hex(Some(spec.background)).as_deref(), Some("#1a1b26"));
    assert_eq!(hex(spec.cursor_text).as_deref(), Some("#1a1b26"));
    assert_eq!(hex(Some(spec.ansi[13])).as_deref(), Some("#bb9af7"));
    assert_eq!(
        skipped(&setup),
        [
            SkipReason::OptionKey,
            SkipReason::Padding,
            SkipReason::Scrollback,
            SkipReason::KeyBindings,
        ]
    );
    assert!(
        setup
            .unknown
            .contains(&"remote_control_password".to_owned())
    );
    assert_no_secret(&setup, "hunter2");
}

#[test]
fn alacritty_reads_imports_first_and_the_main_file_wins() {
    let setup = alacritty::read(&roots("alacritty")).expect("alacritty config");
    assert_eq!(setup.font_family.as_deref(), Some("Menlo"));
    assert_eq!(setup.font_size, Some(16.0));
    assert_eq!(setup.line_height, Some(LineHeight::Points(4.0)));
    assert_eq!(setup.cursor_shape, Some(CursorShapePref::Underline));
    assert_eq!(setup.cursor_blink, Some(CursorBlinkPref::Normal));
    assert_eq!(setup.copy_on_select, Some(true));
    let spec = spec(&setup);
    // The main file's background overrides the imported theme's.
    assert_eq!(hex(Some(spec.background)).as_deref(), Some("#1d1f21"));
    assert_eq!(hex(Some(spec.foreground)).as_deref(), Some("#c5c8c6"));
    assert_eq!(hex(Some(spec.ansi[9])).as_deref(), Some("#d54e53"));
    // `CellForeground` is a mode, not a color, and not an error.
    assert_eq!(spec.cursor, None);
    assert_eq!(
        skipped(&setup),
        [
            SkipReason::Padding,
            SkipReason::OptionKey,
            SkipReason::Scrollback,
        ]
    );
    assert!(setup.unknown.contains(&"window.opacity".to_owned()));
    assert!(setup.unknown.contains(&"env".to_owned()));
    assert_eq!(setup.files.len(), 2);
    assert_no_secret(&setup, "ghp_");
}

#[test]
fn alacritty_yaml_is_named_not_parsed() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".config/alacritty");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alacritty.yml"), "font:\n  size: 20\n").unwrap();
    let setup = alacritty::read(&Roots::in_home(temp.path())).unwrap();
    assert_eq!(setup.font_size, None);
    assert_eq!(skipped(&setup), [SkipReason::LegacyFormat]);
}

#[test]
fn wezterm_reads_literals_and_never_guesses_at_code() {
    let setup = wezterm::read(&roots("wezterm")).expect("wezterm config");
    assert_eq!(setup.font_family.as_deref(), Some("Iosevka Term"));
    // A later computed assignment means the literal may not hold.
    assert_eq!(setup.font_size, None);
    // Not 2.0 from inside an `if`, nor anything set in a callback.
    assert_eq!(setup.line_height, Some(LineHeight::Scale(1.3)));
    assert_eq!(setup.cursor_shape, Some(CursorShapePref::Underline));
    assert_eq!(setup.cursor_blink, Some(CursorBlinkPref::Off));
    let theme = setup.theme.as_ref().unwrap();
    assert_eq!(theme.name, "My Scheme");
    assert_eq!(hex(Some(spec(&setup).background)).as_deref(), Some("#1f1f28"));
    assert_eq!(hex(Some(spec(&setup).ansi[9])).as_deref(), Some("#e82424"));
    let reasons = skipped(&setup);
    for reason in [
        SkipReason::OptionKey,
        SkipReason::Scrollback,
        SkipReason::Padding,
        SkipReason::KeyBindings,
        SkipReason::Unreadable,
    ] {
        assert!(reasons.contains(&reason), "{reason:?} in {reasons:?}");
    }
    assert!(setup.unknown.contains(&"enable_tab_bar".to_owned()));
    // Fields of nested tables are not settings.
    assert!(!setup.unknown.iter().any(|key| key == "left" || key == "key"));
    assert_no_secret(&setup, "sk-live");
}

#[test]
fn wezterm_reads_a_returned_table() {
    let text = "local wezterm = require 'wezterm'\nreturn {\n  font_size = 11,\n  color_scheme = \"Nord (Gogh)\",\n  window_padding = { left = 1 },\n  hide_tab_bar_if_only_one_tab = true,\n}\n";
    let found = wezterm::assignment_keys(text);
    assert_eq!(
        found,
        [
            "font_size",
            "color_scheme",
            "window_padding",
            "hide_tab_bar_if_only_one_tab"
        ]
    );
}

#[test]
fn iterm_reads_the_default_profile_not_the_first() {
    let setup = iterm::read(&roots("iterm")).expect("iterm prefs");
    assert_eq!(setup.font_family.as_deref(), Some("JetBrainsMono-Regular"));
    assert_eq!(setup.font_size, Some(15.0));
    assert_eq!(setup.line_height, Some(LineHeight::Scale(1.1)));
    assert_eq!(setup.cursor_shape, Some(CursorShapePref::Bar));
    assert_eq!(setup.cursor_blink, Some(CursorBlinkPref::Normal));
    assert_eq!(setup.copy_on_select, Some(true));
    assert_eq!(setup.theme.as_ref().unwrap().name, "iTerm2 Work");
    let spec = spec(&setup);
    assert_eq!(hex(Some(spec.background)), hex(parse_color("#0d0f12")));
    assert_eq!(
        skipped(&setup),
        [
            SkipReason::OptionKey,
            SkipReason::Padding,
            SkipReason::Scrollback,
            SkipReason::KeyBindings,
        ]
    );
    // iTerm2 writes every key; the rest are not listed one by one.
    assert!(setup.unknown.is_empty());
    assert_no_secret(&setup, "hunter2");
}

#[test]
fn terminal_decodes_archived_fonts_and_colors() {
    let setup = terminal_app::read(&roots("terminal")).expect("terminal prefs");
    assert_eq!(setup.font_family.as_deref(), Some("SFMono-Regular"));
    assert_eq!(setup.font_size, Some(12.0));
    assert_eq!(setup.line_height, Some(LineHeight::Scale(1.2)));
    assert_eq!(setup.cursor_shape, Some(CursorShapePref::Bar));
    assert_eq!(setup.cursor_blink, Some(CursorBlinkPref::Normal));
    let spec = spec(&setup);
    // A gray-space color (NSWhite) and an RGB one.
    assert_eq!(hex(Some(spec.background)).as_deref(), Some("#000000"));
    assert_eq!(hex(Some(spec.foreground)).as_deref(), Some("#f2f2f2"));
    assert_eq!(setup.theme.as_ref().unwrap().name, "Terminal Pro");
    assert_eq!(skipped(&setup), [SkipReason::OptionKey]);
}

#[test]
fn an_untouched_terminal_is_not_a_setup() {
    assert_eq!(terminal_app::read(&roots("terminal-stock")), None);
}

#[test]
fn no_config_means_no_terminal() {
    let temp = tempfile::tempdir().unwrap();
    assert!(scan(&Roots::in_home(temp.path())).is_empty());
}

#[test]
fn scan_finds_each_fixture_terminal() {
    for (home, source) in [
        ("ghostty", Source::Ghostty),
        ("kitty", Source::Kitty),
        ("alacritty", Source::Alacritty),
        ("wezterm", Source::WezTerm),
        ("iterm", Source::ITerm),
        ("terminal", Source::TerminalApp),
    ] {
        let found = scan(&roots(home));
        assert_eq!(
            found.iter().map(|found| found.source).collect::<Vec<_>>(),
            [source],
            "{home}"
        );
    }
}

#[test]
fn postscript_and_nerd_font_names_find_installed_families() {
    let installed = ["JetBrains Mono", "SF Mono", "Hack Nerd Font Mono", "Menlo"];
    let find = |name| match_font_family(name, installed.iter().copied());
    assert_eq!(find("JetBrainsMono-Regular").as_deref(), Some("JetBrains Mono"));
    assert_eq!(find("SFMono-Regular").as_deref(), Some("SF Mono"));
    assert_eq!(find("menlo").as_deref(), Some("Menlo"));
    assert_eq!(find("HackNFM-Regular").as_deref(), Some("Hack Nerd Font Mono"));
    assert_eq!(find("Comic Code"), None);
    assert_eq!(find(""), None);
}

#[test]
fn the_plan_lists_exactly_what_changes() {
    let setup = ghostty::read(&roots("ghostty")).unwrap();
    let plan = plan(Source::Ghostty, &setup, &Prefs::default(), &env());
    let settings: Vec<&str> = plan.changes.iter().map(|change| change.setting).collect();
    assert_eq!(
        settings,
        [
            t("terminal_import.setting.font"),
            t("terminal_import.setting.size"),
            t("terminal_import.setting.line_height"),
            t("terminal_import.setting.theme"),
            t("terminal_import.setting.cursor"),
            t("terminal_import.setting.copy_on_select"),
        ]
    );
    assert_eq!(plan.apply.font_family.as_deref(), Some("JetBrains Mono"));
    assert_eq!(plan.apply.font_size, Some(15.0));
    assert_eq!(plan.apply.line_height, Some(1.2));
    assert!(matches!(plan.apply.theme, Some(ThemeTarget::Import(_))));
    let theme = &plan.changes[3];
    assert_eq!(theme.to, "Night Owl Custom");
    assert!(theme.swatch.is_some());
    // Four named reasons plus one line for the unknown keys.
    assert_eq!(plan.not_imported.len(), 5, "{:?}", plan.not_imported);

    // Applied, the same plan has nothing left to change but the theme,
    // whose file only exists once applied for real.
    let mut after = Prefs::default();
    let id = match &plan.apply.theme {
        Some(ThemeTarget::Import(spec)) => imported_id(spec),
        _ => unreachable!(),
    };
    plan.apply.write(&mut after, Some(&id));
    after.normalize();
    let again = super::plan(Source::Ghostty, &setup, &after, &env());
    let left: Vec<&str> = again.changes.iter().map(|change| change.setting).collect();
    assert_eq!(left, [t("terminal_import.setting.theme")]);
    assert_eq!(again.unchanged.len(), 5);
}

#[test]
fn out_of_range_sizes_are_shown_as_diri_will_store_them() {
    let setup = Setup {
        font_size: Some(36.0),
        line_height: Some(LineHeight::Points(4.0)),
        ..Setup::default()
    };
    let plan = plan(Source::Kitty, &setup, &Prefs::default(), &env());
    assert_eq!(plan.apply.font_size, Some(Prefs::MAX_TERMINAL_FONT_SIZE));
    assert_eq!(plan.changes[0].to, "20 pt");
    // 4 extra points on the 20 pt Diri draws, rounded to tenths.
    assert_eq!(plan.apply.line_height, Some(1.2));
}

#[test]
fn a_missing_font_is_not_imported_and_says_so() {
    let setup = Setup {
        font_family: Some("Comic Code".into()),
        ..Setup::default()
    };
    let plan = plan(Source::Ghostty, &setup, &Prefs::default(), &env());
    assert!(plan.is_empty());
    assert_eq!(plan.apply.font_family, None);
    assert_eq!(plan.not_imported.len(), 1);
    assert!(plan.not_imported[0].contains("Comic Code"));
}

#[test]
fn a_theme_diri_already_has_is_selected_not_copied() {
    let nord = TermTheme::NORD;
    let setup = Setup {
        theme: Some(ThemeChoice {
            name: "nord-from-elsewhere".into(),
            spec: Some(ThemeSpec {
                name: "nord-from-elsewhere".into(),
                appearance: None,
                background: nord.background,
                foreground: nord.foreground,
                cursor: None,
                cursor_text: None,
                selection: None,
                ansi: nord.ansi,
            }),
        }),
        ..Setup::default()
    };
    let plan = plan(Source::Kitty, &setup, &Prefs::default(), &env());
    assert_eq!(plan.apply.theme, Some(ThemeTarget::Existing(nord.id)));
    // A name alone (a WezTerm built-in scheme) matches the catalog by name.
    let named = Setup {
        theme: Some(ThemeChoice {
            name: nord.name.to_owned(),
            spec: None,
        }),
        ..Setup::default()
    };
    let plan = super::plan(Source::WezTerm, &named, &Prefs::default(), &env());
    assert_eq!(plan.apply.theme, Some(ThemeTarget::Existing(nord.id)));
}

#[test]
fn terminal_comes_last_and_plans_with_nothing_to_change_drop_out() {
    let mut found = scan(&roots("terminal"));
    found.extend(scan(&roots("kitty")));
    found.push(Detected {
        source: Source::Alacritty,
        setup: Setup {
            unknown: vec!["env".into()],
            ..Setup::default()
        },
        modified: None,
    });
    let plans = plans(&found, &Prefs::default(), &env());
    let order: Vec<Source> = plans.iter().map(|plan| plan.source).collect();
    assert_eq!(order, [Source::Kitty, Source::TerminalApp]);
}

/// The Ghostty fixture's plan with its theme renamed and recolored, so tests
/// running in parallel never share (or match) one imported theme.
fn ghostty_plan(theme: &str, background: &str) -> Plan {
    let mut setup = ghostty::read(&roots("ghostty")).unwrap();
    let choice = setup.theme.as_mut().unwrap();
    choice.name = theme.to_owned();
    let spec = choice.spec.as_mut().unwrap();
    spec.name = theme.to_owned();
    spec.background = parse_color(background).unwrap();
    let plan = plan(Source::Ghostty, &setup, &Prefs::default(), &env());
    assert!(matches!(plan.apply.theme, Some(ThemeTarget::Import(_))));
    plan
}

#[test]
fn apply_then_undo_restores_preferences_and_the_themes_folder() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("themes");
    let plan = ghostty_plan("Apply Then Undo", "#010203");
    let before = Prefs::default();
    let mut prefs = before.clone();
    let record = apply(&plan, &dir, &before, |edit| {
        edit(&mut prefs);
        prefs.normalize();
        Ok(())
    })
    .expect("applied");
    assert_eq!(prefs.terminal_font_family, "JetBrains Mono");
    assert_eq!(prefs.terminal_font_size, 15.0);
    assert_eq!(prefs.terminal_cursor_shape, CursorShapePref::Bar);
    assert_eq!(prefs.terminal_cursor_blink, CursorBlinkPref::Off);
    assert!(prefs.terminal_copy_on_select);
    assert_eq!(prefs.terminal_theme, "imported-apply-then-undo");
    let file = dir.join("apply-then-undo.diritheme");
    assert!(file.is_file());
    assert!(themes::find("imported-apply-then-undo").is_some());

    undo(&record, |edit| {
        edit(&mut prefs);
        Ok(())
    })
    .unwrap();
    assert_eq!(prefs, before);
    assert!(!file.exists());
    assert!(themes::find("imported-apply-then-undo").is_none());
}

#[test]
fn a_failed_preference_write_leaves_no_theme_behind() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("themes");
    let plan = ghostty_plan("Failed Write", "#040506");
    let result = apply(&plan, &dir, &Prefs::default(), |_| {
        Err(std::io::Error::other("disk full"))
    });
    assert!(matches!(result, Err(ApplyError::Prefs)));
    assert!(!dir.join("failed-write.diritheme").exists());
}

#[test]
fn undo_puts_back_a_theme_file_the_import_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("themes");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("replaced-theme.diritheme");
    let plan = ghostty_plan("Replaced Theme", "#070809");
    let mut older = match &plan.apply.theme {
        Some(ThemeTarget::Import(spec)) => (**spec).clone(),
        _ => unreachable!(),
    };
    older.background = parse_color("#222222").unwrap();
    let original = theme_file::write_diri(&older);
    std::fs::write(&file, &original).unwrap();

    let mut prefs = Prefs::default();
    let record = apply(&plan, &dir, &Prefs::default(), |edit| {
        edit(&mut prefs);
        Ok(())
    })
    .unwrap();
    assert_ne!(std::fs::read_to_string(&file).unwrap(), original);
    undo(&record, |edit| {
        edit(&mut prefs);
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
}

#[test]
fn source_files_are_never_written() {
    let home = fixture_home("ghostty");
    let config = home.join(".config/ghostty/config");
    let before = std::fs::read(&config).unwrap();
    let modified = std::fs::metadata(&config).unwrap().modified().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut prefs = Prefs::default();
    let plan = ghostty_plan("Read Only Source", "#0a0b0c");
    apply(&plan, &temp.path().join("themes"), &Prefs::default(), |edit| {
        edit(&mut prefs);
        Ok(())
    })
    .unwrap();
    assert_eq!(std::fs::read(&config).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&config).unwrap().modified().unwrap(),
        modified
    );
}

/// What this Mac's terminals would import, and how long reading them takes.
/// Prints setting names and sources only, never values.
#[test]
#[ignore = "reads the real home folder"]
fn live_scan_timing() {
    let roots = Roots::current_user();
    let mut times = Vec::new();
    let mut found = Vec::new();
    for _ in 0..20 {
        let start = std::time::Instant::now();
        found = scan(&roots);
        times.push(start.elapsed());
    }
    times.sort();
    println!(
        "scan median {:?} max {:?}",
        times[times.len() / 2],
        times[times.len() - 1]
    );
    for detected in &found {
        println!(
            "{}: {} files, {} skipped, {} unknown",
            detected.source.name(),
            detected.setup.files.len(),
            detected.setup.skipped.len(),
            detected.setup.unknown.len()
        );
    }
}

#[test]
fn blink_intervals_become_the_nearest_speed() {
    assert_eq!(blink_from_cycle(0.0), CursorBlinkPref::Off);
    assert_eq!(blink_from_cycle(0.6), CursorBlinkPref::Fast);
    // kitty's and Alacritty's ~0.5-0.75 s phases are Diri's normal cycle.
    assert_eq!(blink_from_cycle(1.0), CursorBlinkPref::Normal);
    assert_eq!(blink_from_cycle(1.5), CursorBlinkPref::Slow);
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".config/kitty");
    std::fs::create_dir_all(&dir).unwrap();
    for (interval, expected) in [
        ("0.3", CursorBlinkPref::Fast),
        ("-1", CursorBlinkPref::Normal),
        ("1.0 ease-in-out", CursorBlinkPref::Slow),
        ("0", CursorBlinkPref::Off),
    ] {
        std::fs::write(
            dir.join("kitty.conf"),
            format!("cursor_blink_interval {interval}\n"),
        )
        .unwrap();
        let setup = kitty::read(&Roots::in_home(temp.path())).unwrap();
        assert_eq!(setup.cursor_blink, Some(expected), "{interval}");
    }
}

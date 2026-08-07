use super::*;

#[test]
fn truecolor_and_ansi_palettes_keep_role_semantics_but_differ_surfaces() {
    let tc = Theme::truecolor();
    let ansi = Theme::ansi();
    assert!(matches!(tc.raised, Color::Rgb(22, 24, 28)));
    assert!(matches!(ansi.raised, Color::Indexed(_)));
    assert!(matches!(tc.ember, Color::Rgb(255, 140, 60)));
    assert_eq!(ansi.ember, Color::Indexed(208));
}

#[test]
fn ansi_surfaces_are_three_distinct_elevation_levels() {
    // They used to be `Black`, `Black`, `Black` — tool cards, bubbles and
    // insets were indistinguishable outside a truecolor terminal.
    let ansi = Theme::ansi();
    assert_ne!(ansi.raised, ansi.overlay);
    assert_ne!(ansi.overlay, ansi.hover);
    assert_ne!(ansi.raised, ansi.hover);
}

#[test]
fn every_spinner_frame_is_exactly_one_visible_cell() {
    // Two of the ten braille frames used to be the empty string, so the
    // throbber blinked out twice per cycle and read as a stall.
    for set in [SPINNER_UNICODE, SPINNER_ASCII] {
        for (i, frame) in set.iter().enumerate() {
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(*frame),
                1,
                "frame {i} ({frame:?}) is not one cell wide"
            );
        }
    }
}

#[test]
fn the_ascii_theme_answers_only_in_ascii() {
    let theme = Theme::truecolor().ascii_glyphs();
    let (filled, unfilled) = theme.gauge_symbols();
    let mut glyphs = vec![
        theme.icon_ok(),
        theme.icon_error(),
        theme.marker_selected(),
        theme.ellipsis(),
        theme.assistant_gutter().0,
        theme.assistant_gutter().1,
        theme.border_horizontal(),
        theme.border_vertical(),
        filled,
        unfilled,
    ];
    let border = theme.block_border_set();
    glyphs.extend([
        border.top_left,
        border.top_right,
        border.bottom_left,
        border.bottom_right,
        border.vertical_left,
        border.vertical_right,
        border.horizontal_top,
        border.horizontal_bottom,
    ]);
    let (tl, tr, bl, br) = theme.rounded_corners();
    glyphs.extend([tl, tr, bl, br]);
    glyphs.extend(theme.spinner_frames());
    for glyph in glyphs {
        assert!(glyph.is_ascii(), "{glyph:?} is not ASCII");
    }
}

#[test]
fn the_glyph_set_is_part_of_the_memo_key() {
    // Otherwise a terminal switching capability would keep serving rows
    // drawn with the glyphs it can't show.
    assert_ne!(Theme::ansi(), Theme::ansi().ascii_glyphs());
}

#[test]
fn ansi_text_levels_stay_readable_against_the_surfaces() {
    let ansi = Theme::ansi();
    for text in [ansi.primary, ansi.secondary, ansi.disabled] {
        for surface in [ansi.raised, ansi.overlay, ansi.hover] {
            assert_ne!(text, surface, "text would be invisible on this surface");
        }
    }
}

// --- WCAG ------------------------------------------------------------

/// Tokens that carry running text, and so owe the full AA ratio.
const BODY_TOKENS: &[&str] = &[
    "primary",
    "secondary",
    "ember",
    "amber",
    "success",
    "danger",
    "warning",
    "info",
    "plan",
];
/// Tokens that only ever carry de-emphasised chrome — gutters, borders,
/// gauge tracks, elapsed times, diff line numbers — none of which is the
/// sole carrier of any information. AA's non-text / large-text ratio.
const CHROME_TOKENS: &[&str] = &["disabled"];
const SURFACE_TOKENS: &[&str] = &["base", "raised", "overlay", "hover"];
const AA_BODY: f64 = 4.5;
const AA_LARGE: f64 = 3.0;

fn all_presets() -> Vec<(String, Theme)> {
    let mut out = Vec::new();
    for name in ThemeName::ALL {
        for truecolor in [true, false] {
            let depth = if truecolor { "truecolor" } else { "256/16" };
            out.push((
                format!("{} ({depth})", name.as_str()),
                Theme::preset(name, truecolor),
            ));
        }
    }
    out
}

fn ratio(theme: &Theme, fg: &str, bg: &str) -> f64 {
    contrast_ratio(theme.token(fg).unwrap(), theme.token(bg).unwrap())
        .unwrap_or_else(|| panic!("{fg}/{bg} is not measurable"))
}

/// The gate the whole module exists to satisfy. Every foreground token,
/// on every surface it can land on, in every preset.
#[test]
fn every_preset_meets_wcag_aa() {
    // Foreground/background pairs the renderer actually produces beyond
    // the surface sweep: `components::diff` paints `+` and `-` rows with
    // these exact combinations.
    let diff_pairs = [("success", "diff_add_bg"), ("danger", "diff_del_bg")];
    let mut failures = Vec::new();
    for (label, theme) in all_presets() {
        for (tokens, floor) in [(BODY_TOKENS, AA_BODY), (CHROME_TOKENS, AA_LARGE)] {
            for fg in tokens {
                for bg in SURFACE_TOKENS {
                    let r = ratio(&theme, fg, bg);
                    if r < floor {
                        failures.push(format!("{label}: {fg} on {bg} is {r:.2}:1 < {floor}"));
                    }
                }
            }
        }
        for (fg, bg) in diff_pairs {
            let r = ratio(&theme, fg, bg);
            if r < AA_BODY {
                failures.push(format!("{label}: {fg} on {bg} is {r:.2}:1 < {AA_BODY}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "fix the colour, never the threshold:\n{}",
        failures.join("\n")
    );
}

/// The sweep above must never be able to pass by *skipping* a pair, which
/// is what a `Color::Reset` or a named ANSI variant would make it do.
#[test]
fn every_preset_token_is_measurable() {
    for (label, theme) in all_presets() {
        for token in TOKEN_NAMES {
            let color = theme.token(token).unwrap();
            assert!(
                rgb_of(color).is_some(),
                "{label}: {token} = {color:?} has no measurable value"
            );
        }
    }
}

#[test]
fn every_token_is_reachable_by_name() {
    let mut theme = Theme::truecolor();
    for (i, token) in TOKEN_NAMES.iter().enumerate() {
        let color = Color::Rgb(i as u8, 0, 0);
        theme.set_token(token, color).unwrap();
        assert_eq!(
            theme.token(token),
            Some(color),
            "{token} did not round-trip"
        );
    }
    assert_eq!(
        TOKEN_NAMES.len(),
        16,
        "a token was added to Theme without being added to TOKEN_NAMES"
    );
}

#[test]
fn contrast_ratio_matches_the_wcag_reference_values() {
    let white = Color::Rgb(255, 255, 255);
    let black = Color::Rgb(0, 0, 0);
    assert!((contrast_ratio(black, white).unwrap() - 21.0).abs() < 0.001);
    assert!((contrast_ratio(white, white).unwrap() - 1.0).abs() < 0.001);
    // The ratio is symmetric: the lighter colour is always the numerator.
    assert_eq!(contrast_ratio(black, white), contrast_ratio(white, black));
    // #767676 is the canonical "smallest grey that passes AA on white".
    let grey = parse_hex("#767676").unwrap();
    assert!((contrast_ratio(grey, white).unwrap() - 4.54).abs() < 0.01);
    // A colour the terminal owns cannot be measured.
    assert_eq!(contrast_ratio(Color::Reset, white), None);
}

#[test]
fn the_256_colour_cube_maps_the_way_xterm_does() {
    assert_eq!(indexed_rgb(0), (0, 0, 0));
    assert_eq!(indexed_rgb(15), (255, 255, 255));
    // Cube corners and one interior value.
    assert_eq!(indexed_rgb(16), (0, 0, 0));
    assert_eq!(indexed_rgb(231), (255, 255, 255));
    assert_eq!(indexed_rgb(208), (255, 135, 0));
    assert_eq!(indexed_rgb(75), (95, 175, 255));
    // Greyscale ramp: 8, then every tenth up to 238.
    assert_eq!(indexed_rgb(232), (8, 8, 8));
    assert_eq!(indexed_rgb(255), (238, 238, 238));
}

// --- presets ---------------------------------------------------------

/// `base` has to sit at the bottom of the ladder in a dark theme and at
/// the top of it in a light one, or "raised" means nothing. Ties are
/// allowed: `high_contrast_ansi` deliberately has none.
#[test]
fn the_elevation_ladder_climbs_away_from_the_page_in_every_preset() {
    for (label, theme) in all_presets() {
        let l = |token: &str| relative_luminance(rgb_of(theme.token(token).unwrap()).unwrap());
        let (base, raised, overlay, hover) = (l("base"), l("raised"), l("overlay"), l("hover"));
        let light = l("primary") < base; // dark text ⇒ light theme
        let climbs = if light {
            base >= raised && raised >= overlay && overlay >= hover
        } else {
            base <= raised && raised <= overlay && overlay <= hover
        };
        assert!(
            climbs,
            "{label}: elevation ladder is out of order \
                 (base {base:.4}, raised {raised:.4}, overlay {overlay:.4}, hover {hover:.4})"
        );
    }
}

#[test]
fn the_light_preset_is_a_repick_and_not_an_inversion() {
    let dark = Theme::truecolor();
    let light = Theme::light();
    let lum = |c: Color| relative_luminance(rgb_of(c).unwrap());
    assert!(lum(light.base) > 0.8, "a light theme needs a light page");
    assert!(lum(dark.base) < 0.05);
    // An inversion would leave the accents at the same hue and lightness
    // complement; a repick lands them somewhere else entirely.
    for token in [
        "ember", "amber", "success", "danger", "warning", "info", "plan",
    ] {
        let (d, l) = (
            lum(dark.token(token).unwrap()),
            lum(light.token(token).unwrap()),
        );
        assert!(
                l < d,
                "{token}: a light theme's accents must be darker than a dark theme's ({l:.3} vs {d:.3})"
            );
    }
}

#[test]
fn the_high_contrast_preset_separates_further_than_the_dark_one() {
    let ember = Theme::truecolor();
    let hc = Theme::high_contrast();
    assert!(
        contrast_ratio(hc.primary, hc.base).unwrap()
            > contrast_ratio(ember.primary, ember.base).unwrap()
    );
    // No mid-greys: every neutral is either near-black or well clear of
    // the middle of the range.
    for token in [
        "base",
        "raised",
        "overlay",
        "hover",
        "disabled",
        "secondary",
        "primary",
    ] {
        let l = relative_luminance(rgb_of(hc.token(token).unwrap()).unwrap());
        assert!(
            !(0.06..0.35).contains(&l),
            "{token} sits in the mid-grey band ({l:.3})"
        );
    }
}

#[test]
fn the_16_colour_high_contrast_preset_stays_inside_the_ansi_range() {
    // The whole point of this variant: it has to render on a terminal
    // that has no 256-colour cube at all.
    let hc = Theme::high_contrast_ansi();
    for token in TOKEN_NAMES {
        match hc.token(token).unwrap() {
            Color::Indexed(n) => assert!(n < 16, "{token} = Indexed({n}) is outside 0..16"),
            other => panic!("{token} = {other:?} is not an ANSI index"),
        }
    }
}

// --- config ----------------------------------------------------------

#[test]
fn hex_colours_parse_in_both_lengths_and_with_or_without_the_hash() {
    assert_eq!(parse_hex("#ff8c3c"), Some(Color::Rgb(255, 140, 60)));
    assert_eq!(parse_hex("ff8c3c"), Some(Color::Rgb(255, 140, 60)));
    assert_eq!(parse_hex("  #FF8C3C  "), Some(Color::Rgb(255, 140, 60)));
    // The CSS shorthand expands per digit, not by padding with zeroes.
    assert_eq!(parse_hex("#fff"), Some(Color::Rgb(255, 255, 255)));
    assert_eq!(parse_hex("#08f"), Some(Color::Rgb(0, 136, 255)));
    for bad in [
        "",
        "#",
        "#ff",
        "#ff8c3",
        "#ff8c3cff",
        "red",
        "#gg0000",
        "rgb(1,2,3)",
    ] {
        assert_eq!(parse_hex(bad), None, "{bad:?} should not parse");
    }
}

#[test]
fn resolve_defaults_to_dark_and_applies_overrides() {
    let mut overrides = BTreeMap::new();
    overrides.insert("ember".to_string(), "#00ff00".to_string());
    overrides.insert("base".to_string(), "#123456".to_string());
    let theme = Theme::resolve(None, &overrides).unwrap();
    assert_eq!(theme.ember, Color::Rgb(0, 255, 0));
    assert_eq!(theme.base, Color::Rgb(0x12, 0x34, 0x56));
    // Everything unstated keeps the preset's value.
    assert_eq!(theme.plan, Theme::named(ThemeName::Dark).plan);
}

#[test]
fn resolve_accepts_the_three_names_and_refuses_anything_else() {
    for (input, expected) in [
        ("light", ThemeName::Light),
        ("LIGHT", ThemeName::Light),
        ("high-contrast", ThemeName::HighContrast),
        ("high_contrast", ThemeName::HighContrast),
        (" dark ", ThemeName::Dark),
    ] {
        let theme = Theme::resolve(Some(input), &BTreeMap::new()).unwrap();
        assert_eq!(theme.primary, Theme::named(expected).primary, "{input}");
    }
    // An unknown name must not quietly become the default: the user asked
    // for something and would otherwise never learn they did not get it.
    let err = Theme::resolve(Some("solarized"), &BTreeMap::new()).unwrap_err();
    assert_eq!(err, ThemeError::UnknownName("solarized".into()));
    assert!(err.to_string().contains("high_contrast"), "{err}");
}

#[test]
fn a_bad_override_is_an_error_naming_the_token() {
    let mut overrides = BTreeMap::new();
    overrides.insert("ember".to_string(), "orange".to_string());
    let err = Theme::resolve(None, &overrides).unwrap_err();
    assert_eq!(
        err,
        ThemeError::BadHex {
            token: "ember".into(),
            value: "orange".into()
        }
    );
    assert!(err.to_string().contains("ember"), "{err}");

    let mut overrides = BTreeMap::new();
    overrides.insert("embers".to_string(), "#ffffff".to_string());
    let err = Theme::resolve(None, &overrides).unwrap_err();
    assert_eq!(err, ThemeError::UnknownToken("embers".into()));
    assert!(err.to_string().contains("ember,"), "{err}");
}

#[test]
fn the_palette_is_part_of_the_memo_key() {
    // Same reason as the glyph set: `Theme` keys the transcript memo, so
    // switching preset has to invalidate every cached row.
    assert_ne!(Theme::truecolor(), Theme::light());
    assert_ne!(Theme::truecolor(), Theme::high_contrast());
    let mut recoloured = Theme::truecolor();
    recoloured.base = Color::Rgb(1, 2, 3);
    assert_ne!(Theme::truecolor(), recoloured);
}

// --- the base surface -------------------------------------------------

/// The whole point of the `base` token: after a frame is drawn there must
/// be no cell left showing whatever background the terminal was set to.
/// Lives here rather than in `ui.rs` because this is the token's test, not
/// the layout's.
#[test]
fn no_cell_falls_through_to_the_terminals_own_background() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    let with_text = || {
        vec![crate::app::ChatLine::new(
            crate::app::ChatRole::User,
            "hello".to_string(),
        )]
    };
    let question = || {
        crate::app::Modal::Question(crate::app::QuestionModal {
            question: smith_core::UserQuestion {
                id: "q1".into(),
                prompt: "which one?".into(),
                options: ["a".into(), "b".into(), "c".into()],
            },
            selected: 0,
            custom: String::new(),
        })
    };
    // A modal is the case that nearly got missed: it `Clear`s the pane
    // behind it, and `Clear` resets cells to the terminal's own colours.
    for (label, lines, modal) in [
        ("idle", Vec::new(), crate::app::Modal::None),
        ("transcript", with_text(), crate::app::Modal::None),
        ("modal", with_text(), question()),
    ] {
        let mut app = crate::app::App::new(crate::app::TuiConfig {
            banner: "smith".into(),
            provider_label: "ollama".into(),
            model_label: "qwen2.5".into(),
            cwd_display: "~/smith".into(),
            git_branch: None,
            idle_hint: crate::app::IdleHint::Tip(String::new()),
            initial_lines: lines,
            permission_policy: smith_core::PermissionPolicy::default(),
            // The light preset makes the failure mode real: on a dark
            // terminal an unpainted cell is a black hole in a white page.
            theme: Theme::light(),
            goal: None,
            tasks: Vec::new(),
            commands: crate::slash::SlashRegistry::builtin(),
            keys: Default::default(),
            history: Vec::new(),
            logs: crate::logbuf::LogBuffer::default(),
            console_url: None,
        });
        app.modal = modal;
        // Both width tiers: with the sidebar (>= 80) and without it.
        for (width, height) in [(80u16, 24u16), (60, 20)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            for y in 0..height {
                for x in 0..width {
                    let cell = buffer.cell(Position::new(x, y)).unwrap();
                    assert_ne!(
                            cell.bg,
                            Color::Reset,
                            "{label} at {width}x{height}: cell ({x},{y}) kept the terminal's background"
                        );
                }
            }
        }
    }
}

// --- the rule ---------------------------------------------------------

/// "No `Color::` literal outside `theme.rs`" was a convention. This makes
/// it a build failure. Test modules are exempt: a render assertion needs
/// to name the colour it expects to find in the buffer. A test module is
/// either the inline `#[cfg(test)]` block or a whole `tests.rs`.
#[test]
fn no_colour_literal_lives_outside_this_module() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    assert!(files.len() > 5, "found no sources to check under {src:?}");

    let mut violations = Vec::new();
    for file in files {
        // `theme.rs` is where colours are defined. A `tests.rs` is a test
        // module in its entirety — it is reachable only through a
        // `#[cfg(test)] mod tests;` — so it earns the same exemption the
        // inline block below earns, and it has to be named here rather than
        // detected, because the file carries no `#[cfg(test)]` of its own.
        if file
            .file_name()
            .is_some_and(|n| n == "theme.rs" || n == "tests.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        // Everything from the inline test module down is a test.
        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (i, line) in production.lines().enumerate() {
            if line.contains("Color::") && !line.trim_start().starts_with("//") {
                violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "colours belong in theme.rs:\n{}",
        violations.join("\n")
    );
}

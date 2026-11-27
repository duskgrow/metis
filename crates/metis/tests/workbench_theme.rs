//! Verify the production theme projection without rebuilding workbench state.

#![cfg(feature = "workbench")]

#[path = "../src/workbench/theme.rs"]
mod theme;

use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use gpui_kit::{
    App, AppContext as _, AssetSource, Context, Entity, EntityId, FocusHandle, TestAppContext,
    Window, WindowOptions,
    base::{Root, Theme as BaseTheme, ThemeAppearance, TypographyTokens},
    component::{Theme, ThemeMode},
    div,
    prelude::*,
    px, rgb,
    test::TestWindowExt as _,
};
use theme::{
    MetisThemeSpec, MetisTokens, ProjectedTokens, ResolvedTokens, ThemeController, theme_definition,
};

fn standalone_config(definition: &MetisThemeSpec) -> Theme {
    let mut theme = Theme::default();
    theme.apply_config(&definition.config());
    theme
}

fn assert_config_reload(cx: &App, definition: &MetisThemeSpec) {
    let expected = standalone_config(definition);
    let actual = Theme::global(cx);
    assert_eq!(actual.mode, definition.resolved.mode);
    assert_eq!(actual.colors.primary, expected.colors.primary);
    assert_eq!(
        actual.colors.primary_foreground,
        expected.colors.primary_foreground
    );
    assert_eq!(actual.colors.primary_hover, expected.colors.primary_hover);
    assert_eq!(
        actual.colors.muted_foreground,
        expected.colors.muted_foreground
    );
    assert_eq!(actual.colors.ring, expected.colors.ring);
    assert_eq!(actual.tokens.primary, expected.tokens.primary);
    assert_eq!(actual.tokens.primary_hover, expected.tokens.primary_hover);
    assert_eq!(actual.font_size, expected.font_size);
    assert_eq!(actual.radius, expected.radius);
    assert_eq!(
        actual.colors.primary, definition.resolved.projected.primary,
        "the generated config and resolved snapshot share one source"
    );
    assert_eq!(
        BaseTheme::global(cx).tokens,
        actual.semantic_tokens(),
        "mode reload must still synchronize Base"
    );
}

fn assert_projection(cx: &App, definition: &MetisThemeSpec) {
    let expected = &definition.resolved.projected;
    let theme = Theme::global(cx);
    assert_eq!(theme.mode, definition.resolved.mode);
    assert_eq!(theme.colors.background, expected.background);
    assert_eq!(theme.colors.foreground, expected.foreground);
    assert_eq!(theme.colors.popover, expected.popover);
    assert_eq!(theme.colors.primary, expected.primary);
    assert_eq!(theme.colors.primary_foreground, expected.primary_foreground);
    assert_eq!(theme.colors.primary_hover, expected.primary_hover);
    assert_eq!(theme.colors.muted_foreground, expected.muted_foreground);
    assert_eq!(theme.colors.ring, expected.ring);
    assert_eq!(theme.colors.selection, expected.selection);
    assert_eq!(theme.colors.border, expected.border);
    assert_eq!(theme.tokens.background.color, expected.background);
    assert_eq!(theme.tokens.foreground.color, expected.foreground);
    assert_eq!(theme.tokens.popover.color, expected.popover);
    assert_eq!(theme.tokens.primary.color, expected.primary);
    assert_eq!(theme.tokens.selection.color, expected.selection);
    assert_eq!(theme.tokens.border.color, expected.border);
    assert_eq!(theme.tokens.primary.background, expected.primary.into());
    assert_eq!(theme.radius, expected.radius);
    assert_eq!(theme.radius_lg, expected.radius_lg);
    assert_eq!(theme.font_size, expected.font_size);
    assert_eq!(theme.mono_font_size, expected.mono_font_size);
    assert_eq!(theme.shadow, expected.shadow);

    let base = BaseTheme::global(cx);
    assert_eq!(base.tokens, theme.semantic_tokens());
    assert_eq!(base.tokens.colors.background, expected.background);
    assert_eq!(base.tokens.colors.surface, expected.popover);
    assert_eq!(base.tokens.colors.primary, expected.primary);
    assert_eq!(
        base.tokens.colors.primary_foreground,
        expected.primary_foreground
    );
    assert_eq!(
        base.tokens.colors.muted_foreground,
        expected.muted_foreground
    );
    assert_eq!(base.tokens.colors.ring, expected.ring);
    assert_eq!(base.tokens.colors.selection, expected.selection);
    assert_eq!(base.tokens.colors.border, expected.border);
    assert_eq!(base.tokens.radius.md, expected.radius);
    assert_eq!(base.tokens.radius.lg, expected.radius_lg);
    assert_eq!(base.tokens.typography.md.size, expected.font_size);
    assert_eq!(base.tokens.typography.mono_md.size, expected.mono_font_size);
    assert_eq!(
        base.appearance,
        if definition.resolved.mode.is_dark() {
            ThemeAppearance::Dark
        } else {
            ThemeAppearance::Light
        }
    );
}

#[derive(Clone, Debug, PartialEq)]
struct FocusAnchor {
    role: &'static str,
    logical_id: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct ScrollAnchor {
    item_id: u64,
    offset: gpui_kit::Pixels,
}

#[derive(Clone, Debug, PartialEq)]
struct ContinuousState {
    logical_id: u64,
    focus_anchor: FocusAnchor,
    selection: Range<usize>,
    scroll_anchor: ScrollAnchor,
    draft: String,
}

struct ProbeView {
    controller: Rc<RefCell<ThemeController>>,
    focus: FocusHandle,
    state: ContinuousState,
    rendered_tokens: Option<ResolvedTokens>,
}

impl ProbeView {
    fn state(&self) -> ContinuousState {
        self.state.clone()
    }
}

impl Render for ProbeView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.controller.borrow().resolved().clone();
        self.rendered_tokens = Some(tokens.clone());
        div()
            .size_full()
            .track_focus(&self.focus)
            .bg(tokens.metis.navigation_surface)
            .text_color(tokens.metis.foreground_secondary)
            .text_size(tokens.metis.metadata_font_size)
            .p(tokens.metis.density_gap)
            .child("theme probe")
    }
}

fn refresh_and_assert_preserved(
    handle: gpui_kit::AnyWindowHandle,
    root: &gpui_kit::WindowHandle<Root>,
    view: &Entity<ProbeView>,
    entity_id: EntityId,
    state: &ContinuousState,
    definition: &MetisThemeSpec,
    cx: &mut TestAppContext,
) {
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let view = view.read(cx);
        assert!(view.focus.is_focused(window));
        assert_eq!(view.state(), *state);
        assert_eq!(view.rendered_tokens.as_ref(), Some(&definition.resolved));
    })
    .expect("refresh probe window");
    assert_eq!(view.entity_id(), entity_id);
    let content = root
        .read_with(cx, |root, _| root.view().clone())
        .expect("read Root content");
    assert_eq!(content.entity_id(), entity_id);
}

fn assert_controller_is_the_complete_source(
    controller: &Rc<RefCell<ThemeController>>,
    definition: &MetisThemeSpec,
    cx: &App,
) {
    let controller = controller.borrow();
    assert_eq!(controller.resolved(), &definition.resolved);
    let base = BaseTheme::global(cx);
    assert_ne!(
        controller.resolved().metis.navigation_surface,
        base.tokens.colors.surface,
        "the Metis navigation color is not encoded in Base's surface slot"
    );
    assert_eq!(
        base.tokens.typography.sm.size,
        TypographyTokens::default().sm.size
    );
    assert_ne!(
        controller.resolved().metis.metadata_font_size,
        base.tokens.typography.sm.size,
        "metadata scale cannot be recovered from the legacy typography projection"
    );
    assert_ne!(
        controller.resolved().metis.density_gap,
        base.tokens.spacing.md,
        "density cannot be recovered from the default Base spacing projection"
    );
}

fn confirmed_prototype_tokens(mode: ThemeMode) -> ResolvedTokens {
    let (projected, metis) = match mode {
        ThemeMode::Light => (
            ProjectedTokens {
                background: rgb(0x00f7_f8f9).into(),
                foreground: rgb(0x0020_252b).into(),
                popover: rgb(0x00ff_ffff).into(),
                primary: rgb(0x0039_6d9e).into(),
                primary_foreground: rgb(0x00ff_ffff).into(),
                primary_hover: rgb(0x002e_5d88).into(),
                muted_foreground: rgb(0x0060_6a75).into(),
                ring: rgb(0x0027_6da9).into(),
                selection: rgb(0x00e1_e7ec).into(),
                border: rgb(0x00c6_cdd4).into(),
                radius: px(6.),
                radius_lg: px(8.),
                font_size: px(14.),
                mono_font_size: px(12.),
                shadow: true,
            },
            MetisTokens {
                navigation_surface: rgb(0x00f0_f2f4).into(),
                code_surface: rgb(0x00f3_f5f6).into(),
                foreground_secondary: rgb(0x0048_515c).into(),
                border_subtle: rgb(0x00dc_e1e5).into(),
                status_info: rgb(0x0031_5f8b).into(),
                diff_addition_surface: rgb(0x00e7_f2e9).into(),
                syntax_keyword: rgb(0x006b_4896).into(),
                metadata_font_size: px(12.),
                density_gap: px(8.),
                motion_duration: Duration::from_millis(120),
            },
        ),
        ThemeMode::Dark => (
            ProjectedTokens {
                background: rgb(0x0017_191c).into(),
                foreground: rgb(0x00e7_e9ec).into(),
                popover: rgb(0x0020_2428).into(),
                primary: rgb(0x008a_b7e4).into(),
                primary_foreground: rgb(0x0010_1820).into(),
                primary_hover: rgb(0x00a2_c8eb).into(),
                muted_foreground: rgb(0x0098_9fa9).into(),
                ring: rgb(0x00a9_ccf3).into(),
                selection: rgb(0x002a_3036).into(),
                border: rgb(0x003a_4047).into(),
                radius: px(6.),
                radius_lg: px(8.),
                font_size: px(14.),
                mono_font_size: px(12.),
                shadow: true,
            },
            MetisTokens {
                navigation_surface: rgb(0x001b_1e21).into(),
                code_surface: rgb(0x001c_2024).into(),
                foreground_secondary: rgb(0x00b9_bec6).into(),
                border_subtle: rgb(0x002b_3035).into(),
                status_info: rgb(0x00a5_c4e4).into(),
                diff_addition_surface: rgb(0x001d_2b24).into(),
                syntax_keyword: rgb(0x00b9_a8e2).into(),
                metadata_font_size: px(12.),
                density_gap: px(8.),
                motion_duration: Duration::from_millis(120),
            },
        ),
    };
    ResolvedTokens {
        mode,
        projected,
        metis,
    }
}

#[test]
fn workbench_theme_keeps_the_confirmed_prototype_baseline() {
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        assert_eq!(
            theme_definition(mode).resolved,
            confirmed_prototype_tokens(mode)
        );
    }
}

#[gpui_kit::test]
fn workbench_theme_projection_survives_mode_reload_without_rebuilding_state(
    cx: &mut TestAppContext,
) {
    let light = theme_definition(ThemeMode::Light);
    let dark = theme_definition(ThemeMode::Dark);

    cx.update(gpui_kit::init);
    let controller =
        cx.update(|cx| Rc::new(RefCell::new(ThemeController::new(ThemeMode::Light, cx))));
    let (handle, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|cx| ProbeView {
                    controller: Rc::clone(&controller),
                    focus: cx.focus_handle(),
                    state: ContinuousState {
                        logical_id: 41,
                        focus_anchor: FocusAnchor {
                            role: "Composer",
                            logical_id: 41,
                        },
                        selection: 3..11,
                        scroll_anchor: ScrollAnchor {
                            item_id: 99,
                            offset: px(37.),
                        },
                        draft: "unsent draft".to_owned(),
                    },
                    rendered_tokens: None,
                })
            })
        })
        .expect("open theme probe window");
    let root = handle.downcast::<Root>().expect("gpui-kit installs Root");
    let entity_id = view.entity_id();
    let content = root
        .read_with(cx, |root, _| root.view().clone())
        .expect("read initial Root content");
    assert_eq!(content.entity_id(), entity_id);
    let state = view.read_with(cx, |view, _| view.state());

    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        window.render_frame(cx);
        assert!(focus.is_focused(window));
    })
    .expect("focus probe view");
    cx.update(|cx| {
        assert_projection(cx, &light);
        assert_controller_is_the_complete_source(&controller, &light, cx);
    });
    refresh_and_assert_preserved(handle, &root, &view, entity_id, &state, &light, cx);

    cx.update(|cx| {
        controller.borrow_mut().begin_mode(ThemeMode::Dark, cx);
        assert_config_reload(cx, &dark);
        controller.borrow().finish_mode(cx);
        assert_projection(cx, &dark);
        assert_controller_is_the_complete_source(&controller, &dark, cx);
    });
    refresh_and_assert_preserved(handle, &root, &view, entity_id, &state, &dark, cx);

    cx.update(|cx| {
        controller.borrow_mut().set_mode(ThemeMode::Light, cx);
        assert_projection(cx, &light);
        assert_controller_is_the_complete_source(&controller, &light, cx);
    });
    refresh_and_assert_preserved(handle, &root, &view, entity_id, &state, &light, cx);
}

gpui_kit::assets::icon_assets!(ProbeAssets, [Search, Check]);

#[test]
fn workbench_theme_selective_icon_sources_load_their_real_svg_bytes() {
    use gpui_kit::assets::IconName;

    for icon in [IconName::Search, IconName::Check] {
        let path = icon.path();
        let bytes = ProbeAssets
            .load(&path)
            .expect("load selected icon")
            .unwrap_or_else(|| panic!("selected icon is missing: {path}"));
        assert!(!bytes.is_empty(), "selected icon is empty: {path}");
    }
    let unlisted = IconName::Accessibility.path();
    assert!(
        ProbeAssets
            .load(&unlisted)
            .expect("query unlisted icon")
            .is_none(),
        "a selective source must report unlisted icons instead of pretending to load them"
    );
}

//! Complete Metis semantic tokens and the single GPUI theme write path.

use std::{rc::Rc, time::Duration};

use gpui_kit::{
    App, Hsla, Pixels,
    component::{Theme, ThemeConfig, ThemeMode},
    px, rgb,
};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ProjectedTokens {
    pub(super) background: Hsla,
    pub(super) foreground: Hsla,
    pub(super) popover: Hsla,
    pub(super) primary: Hsla,
    pub(super) primary_foreground: Hsla,
    pub(super) primary_hover: Hsla,
    pub(super) muted_foreground: Hsla,
    pub(super) ring: Hsla,
    pub(super) selection: Hsla,
    pub(super) border: Hsla,
    pub(super) radius: Pixels,
    pub(super) radius_lg: Pixels,
    pub(super) font_size: Pixels,
    pub(super) mono_font_size: Pixels,
    pub(super) shadow: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct MetisTokens {
    pub(super) navigation_surface: Hsla,
    pub(super) code_surface: Hsla,
    pub(super) foreground_secondary: Hsla,
    pub(super) border_subtle: Hsla,
    pub(super) status_info: Hsla,
    pub(super) diff_addition_surface: Hsla,
    pub(super) syntax_keyword: Hsla,
    pub(super) metadata_font_size: Pixels,
    pub(super) density_gap: Pixels,
    pub(super) motion_duration: Duration,
}

/// The complete product snapshot. GPUI receives only `projected`; components
/// read this Rust value rather than trying to recover Metis tokens from GPUI.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ResolvedTokens {
    pub(super) mode: ThemeMode,
    pub(super) projected: ProjectedTokens,
    pub(super) metis: MetisTokens,
}

/// One Rust definition drives both the lossy GPUI config and the complete
/// product snapshot. There is no independently maintained JSON theme value.
pub(super) struct MetisThemeSpec {
    name: &'static str,
    pub(super) resolved: ResolvedTokens,
}

impl MetisThemeSpec {
    pub(super) fn config(&self) -> Rc<ThemeConfig> {
        let tokens = &self.resolved.projected;
        let config = serde_json::json!({
            "name": self.name,
            "mode": if self.resolved.mode.is_dark() { "dark" } else { "light" },
            "font.size": f32::from(tokens.font_size),
            "mono_font.size": f32::from(tokens.mono_font_size),
            "radius": u32::from(tokens.radius),
            "radius.lg": u32::from(tokens.radius_lg),
            "shadow": tokens.shadow,
            "colors": {
                "background": tokens.background,
                "foreground": tokens.foreground,
                "popover.background": tokens.popover,
                "primary.background": tokens.primary,
                "primary.foreground": tokens.primary_foreground,
                "primary.hover.background": tokens.primary_hover,
                "muted.foreground": tokens.muted_foreground,
                "ring": tokens.ring,
                "selection": tokens.selection,
                "border": tokens.border,
            }
        });
        Rc::new(serde_json::from_value(config).expect("serialize Metis theme spec"))
    }
}

pub(super) struct ThemeController {
    resolved: ResolvedTokens,
}

impl ThemeController {
    pub(super) fn new(mode: ThemeMode, cx: &mut App) -> Self {
        let light = theme_definition(ThemeMode::Light);
        let dark = theme_definition(ThemeMode::Dark);
        let current = if mode.is_dark() { &dark } else { &light };
        let resolved = current.resolved.clone();
        let current_config = current.config();
        Theme::update(cx, |theme| {
            theme.light_theme = light.config();
            theme.dark_theme = dark.config();
            theme.apply_config(&current_config);
        });
        let controller = Self { resolved };
        controller.finish_mode(cx);
        controller
    }

    pub(super) fn resolved(&self) -> &ResolvedTokens {
        &self.resolved
    }

    pub(super) fn set_mode(&mut self, mode: ThemeMode, cx: &mut App) {
        self.begin_mode(mode, cx);
        self.finish_mode(cx);
    }

    /// Switch configs first. `Theme::update` reloads the target mode after its
    /// closure, so the lossy projection must be written by `finish_mode`.
    pub(super) fn begin_mode(&mut self, mode: ThemeMode, cx: &mut App) {
        self.resolved = theme_definition(mode).resolved;
        Theme::update(cx, |theme| theme.mode = mode);
    }

    pub(super) fn finish_mode(&self, cx: &mut App) {
        let projected = self.resolved.projected.clone();
        Theme::update(cx, |theme| write_projection(theme, &projected));
    }
}

pub(super) fn theme_definition(mode: ThemeMode) -> MetisThemeSpec {
    let (name, resolved) = match mode {
        ThemeMode::Light => (
            "Metis Light",
            ResolvedTokens {
                mode,
                projected: ProjectedTokens {
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
                metis: MetisTokens {
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
            },
        ),
        ThemeMode::Dark => (
            "Metis Dark",
            ResolvedTokens {
                mode,
                projected: ProjectedTokens {
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
                metis: MetisTokens {
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
            },
        ),
    };
    MetisThemeSpec { name, resolved }
}

fn write_projection(theme: &mut Theme, tokens: &ProjectedTokens) {
    theme.colors.background = tokens.background;
    theme.colors.foreground = tokens.foreground;
    theme.colors.popover = tokens.popover;
    theme.colors.primary = tokens.primary;
    theme.colors.primary_foreground = tokens.primary_foreground;
    theme.colors.primary_hover = tokens.primary_hover;
    theme.colors.muted_foreground = tokens.muted_foreground;
    theme.colors.ring = tokens.ring;
    theme.colors.selection = tokens.selection;
    theme.colors.border = tokens.border;
    theme.radius = tokens.radius;
    theme.radius_lg = tokens.radius_lg;
    theme.font_size = tokens.font_size;
    theme.mono_font_size = tokens.mono_font_size;
    theme.shadow = tokens.shadow;
}

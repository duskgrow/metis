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
    pub(super) action_primary_hover: Hsla,
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
                    background: rgb(0x00f4_f6fa).into(),
                    foreground: rgb(0x0010_1828).into(),
                    popover: rgb(0x00fd_fdfe).into(),
                    primary: rgb(0x0025_47b8).into(),
                    selection: rgb(0x00a9_bff1).into(),
                    border: rgb(0x00b8_c0cf).into(),
                    radius: px(7.),
                    radius_lg: px(11.),
                    font_size: px(17.),
                    mono_font_size: px(14.),
                    shadow: false,
                },
                metis: MetisTokens {
                    navigation_surface: rgb(0x00e8_ecf3).into(),
                    code_surface: rgb(0x00ed_f1f7).into(),
                    foreground_secondary: rgb(0x004d_596f).into(),
                    action_primary_hover: rgb(0x001c_3891).into(),
                    status_info: rgb(0x0024_74a6).into(),
                    diff_addition_surface: rgb(0x00df_f2e3).into(),
                    syntax_keyword: rgb(0x0086_46c5).into(),
                    metadata_font_size: px(13.),
                    density_gap: px(7.),
                    motion_duration: Duration::from_millis(90),
                },
            },
        ),
        ThemeMode::Dark => (
            "Metis Dark",
            ResolvedTokens {
                mode,
                projected: ProjectedTokens {
                    background: rgb(0x000c_1119).into(),
                    foreground: rgb(0x00ed_f1f8).into(),
                    popover: rgb(0x0013_1a25).into(),
                    primary: rgb(0x0091_aaf7).into(),
                    selection: rgb(0x0040_5b91).into(),
                    border: rgb(0x0041_4e63).into(),
                    radius: px(8.),
                    radius_lg: px(12.),
                    font_size: px(18.),
                    mono_font_size: px(15.),
                    shadow: true,
                },
                metis: MetisTokens {
                    navigation_surface: rgb(0x0015_1c27).into(),
                    code_surface: rgb(0x000a_0e15).into(),
                    foreground_secondary: rgb(0x00a2_aec0).into(),
                    action_primary_hover: rgb(0x00ad_c0fa).into(),
                    status_info: rgb(0x0065_b9e8).into(),
                    diff_addition_surface: rgb(0x0017_3524).into(),
                    syntax_keyword: rgb(0x00c5_98f0).into(),
                    metadata_font_size: px(15.),
                    density_gap: px(9.),
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
    theme.colors.selection = tokens.selection;
    theme.colors.border = tokens.border;
    theme.radius = tokens.radius;
    theme.radius_lg = tokens.radius_lg;
    theme.font_size = tokens.font_size;
    theme.mono_font_size = tokens.mono_font_size;
    theme.shadow = tokens.shadow;
}

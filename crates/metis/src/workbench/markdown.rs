//! The only workbench renderer for untrusted agent and tool Markdown.

use std::{rc::Rc, sync::Arc};

use gpui_kit::{
    App, ClickEvent, Entity, ImageSource, MouseButton, Window,
    component::{
        button::Button,
        label::Label,
        text::{MarkdownExtensions, TextView, TextViewState},
    },
    div,
    prelude::*,
    px,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MarkdownEvent {
    LinkClicked(String),
    LinkRejected(String),
    ConfirmationRequested(String),
    Cancelled(String),
    ConfirmationRejected(String),
    ImageRequested(String),
    ImageRejected(String),
}

type Observer = Arc<dyn Fn(MarkdownEvent) + Send + Sync>;
type ConfirmationHandler = Arc<dyn Fn(String, &mut Window, &mut App) + Send + Sync>;
type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;

fn ignore_event(_: MarkdownEvent) {}
fn ignore_confirmation(_: String, _: &mut Window, _: &mut App) {}

#[derive(gpui_kit::IntoElement)]
pub(super) struct AgentMarkdown {
    state: Entity<TextViewState>,
    observer: Observer,
    on_confirmation: ConfirmationHandler,
}

impl AgentMarkdown {
    pub(super) fn new(state: &Entity<TextViewState>) -> Self {
        Self {
            state: state.clone(),
            observer: Arc::new(ignore_event),
            on_confirmation: Arc::new(ignore_confirmation),
        }
    }

    /// Observation is side-effect-free policy telemetry; it is not a Host or
    /// resource-loading callback.
    #[cfg(test)]
    #[allow(dead_code)] // Used by the integration security probe.
    pub(super) fn observe(
        mut self,
        observer: impl Fn(MarkdownEvent) + Send + Sync + 'static,
    ) -> Self {
        self.observer = Arc::new(observer);
        self
    }

    pub(super) fn on_confirmation(
        mut self,
        on_confirmation: impl Fn(String, &mut Window, &mut App) + Send + Sync + 'static,
    ) -> Self {
        self.on_confirmation = Arc::new(on_confirmation);
        self
    }
}

impl RenderOnce for AgentMarkdown {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let link_observer = self.observer.clone();
        let image_observer = self.observer;
        let on_confirmation = self.on_confirmation;
        TextView::new(&self.state)
            .markdown_extensions(MarkdownExtensions::default().raw_html(false))
            .on_link_click(move |url, event, window, cx| {
                let target = url.to_string();
                link_observer(MarkdownEvent::LinkClicked(target.clone()));
                if primary_activation(event) && complete_https(&target) {
                    link_observer(MarkdownEvent::ConfirmationRequested(target.clone()));
                    // A click only requests confirmation. LinkConfirmation is
                    // the sole production caller of `open_url`.
                    on_confirmation(target, window, cx);
                } else {
                    link_observer(MarkdownEvent::LinkRejected(target));
                }
                window.refresh();
            })
            .image_source(move |uri| {
                let uri = uri.to_string();
                image_observer(MarkdownEvent::ImageRequested(uri.clone()));
                let image_observer = image_observer.clone();
                // Custom(None) is authoritative: no resource lookup, data
                // decoding, or fallback to the untrusted source URI.
                ImageSource::Custom(Arc::new(move |_, _| {
                    image_observer(MarkdownEvent::ImageRejected(uri.clone()));
                    None
                }))
            })
    }
}

#[derive(gpui_kit::IntoElement)]
pub(super) struct LinkConfirmation {
    target: String,
    presented_target: Option<String>,
    observer: Observer,
    on_close: Option<CloseHandler>,
}

impl LinkConfirmation {
    pub(super) fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            presented_target: None,
            observer: Arc::new(ignore_event),
            on_close: None,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used by the integration security probe.
    pub(super) fn presented_target(mut self, target: impl Into<String>) -> Self {
        self.presented_target = Some(target.into());
        self
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used by the integration security probe.
    pub(super) fn observe(
        mut self,
        observer: impl Fn(MarkdownEvent) + Send + Sync + 'static,
    ) -> Self {
        self.observer = Arc::new(observer);
        self
    }

    pub(super) fn on_close(mut self, on_close: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(on_close));
        self
    }
}

impl RenderOnce for LinkConfirmation {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let presented_target = self.presented_target.unwrap_or_else(|| self.target.clone());
        let confirm_target = presented_target.clone();
        let target = self.target;
        let confirm_value = target.clone();
        let confirm_observer = self.observer.clone();
        let cancel_observer = self.observer;
        let confirm_close = self.on_close.clone();
        let cancel_close = self.on_close;

        div()
            .id("link-confirmation")
            .absolute()
            .left(px(0.))
            .right(px(0.))
            .bottom(px(0.))
            .p_3()
            .border_1()
            .child(div().id("link-target").child(Label::new(presented_target)))
            .child(
                Button::new("confirm-link")
                    .label("Open confirmed URL")
                    .on_click(move |_, window, cx| {
                        if confirm_target == confirm_value && complete_https(&confirm_value) {
                            cx.open_url(&confirm_value);
                        } else {
                            confirm_observer(MarkdownEvent::ConfirmationRejected(
                                confirm_value.clone(),
                            ));
                        }
                        if let Some(close) = &confirm_close {
                            close(window, cx);
                        }
                        window.refresh();
                    }),
            )
            .child(
                Button::new("cancel-link")
                    .label("Cancel")
                    .on_click(move |_, window, cx| {
                        cancel_observer(MarkdownEvent::Cancelled(target.clone()));
                        if let Some(close) = &cancel_close {
                            close(window, cx);
                        }
                        window.refresh();
                    }),
            )
    }
}

pub(super) fn complete_https(target: &str) -> bool {
    let Some(rest) = target.strip_prefix("https://") else {
        return false;
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    authority_end > 0
        && target
            .chars()
            .all(|character| !character.is_whitespace() && !character.is_control())
}

fn primary_activation(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(event) => event.up.button == MouseButton::Left,
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(event) => !event.long_press,
    }
}

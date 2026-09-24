//! Running subagents above the composer: one pill per subagent still working
//! in the selected chat, so their progress stays visible however far the
//! transcript has scrolled past their spawn chips. A pill opens the same tab
//! as its chip.

use std::rc::Rc;

use gpui::{
    Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    WeakEntity, Window, div, prelude::*, px,
};

use super::Shell;
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::{RunningSubagent, running_subagents};

const PILL_HEIGHT: f32 = 26.0;
const TITLE_MAX_WIDTH: f32 = 180.0;
const PILL_MAX_WIDTH: f32 = 320.0;
/// Pills shrink to share the row (tails truncate first).
const PILL_MIN_WIDTH: f32 = 140.0;
/// Beyond this a count stands in for the rest, so the row never hides agents
/// off-screen; each one is still reachable through its spawn chip.
const MAX_PILLS: usize = 4;
/// With more pills a tail shrinks to a stub ellipsis that reads as noise.
const MAX_PILLS_WITH_TAIL: usize = 3;

/// Its own view so the spinner's frame requests re-render the strip, not the
/// Shell, whose notifications invalidate the cached transcript.
pub(super) struct SubagentStrip {
    shell: WeakEntity<Shell>,
    state: Entity<AppState>,
    width: f32,
    /// Keyed by (selected chat, transcript revision): the Shell re-renders
    /// this view on every composer keystroke.
    running: ((Option<String>, u64), Rc<Vec<RunningSubagent>>),
    _observation: Subscription,
}

impl SubagentStrip {
    pub(super) fn new(
        shell: WeakEntity<Shell>,
        state: Entity<AppState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&state, |strip, _, cx| {
            if strip.refresh(cx) {
                cx.notify();
            }
        });
        let mut strip = Self {
            shell,
            state,
            width: 0.0,
            running: ((None, u64::MAX), Rc::default()),
            _observation: observation,
        };
        strip.refresh(cx);
        strip
    }

    pub(super) fn set_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if (self.width - width).abs() > 0.5 {
            self.width = width;
            cx.notify();
        }
    }

    /// Returns whether the visible list changed.
    fn refresh(&mut self, cx: &mut Context<Self>) -> bool {
        let state = self.state.read(cx);
        let key = (state.selected_chat.clone(), state.transcript_revision);
        if self.running.0 == key {
            return false;
        }
        let running = running_subagents(&state.transcript);
        let changed = *self.running.1 != running;
        self.running = (key, Rc::new(running));
        changed
    }

    fn open(&mut self, agent: &RunningSubagent, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let (doc_id, title) = (agent.doc_id.clone(), agent.title.to_string());
        self.shell
            .update(cx, |shell, cx| {
                shell.add_subagent_surface(chat_id, doc_id, title, false, cx)
            })
            .ok();
    }
}

impl Render for SubagentStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running.1.clone();
        if running.is_empty() {
            return div().into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let view = cx.entity_id();
        let show_tails = running.len() <= MAX_PILLS_WITH_TAIL;
        div()
            .w_full()
            .max_w(px(self.width))
            .mx_auto()
            .px(px(Theme::SPACE_LG))
            .pb(px(6.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .children(
                        running
                            .iter()
                            .take(MAX_PILLS)
                            .enumerate()
                            .map(|(ix, agent)| {
                                let pill_agent = agent.clone();
                                div()
                                    .id(("running-subagent", ix))
                                    .flex_1()
                                    .min_w(px(PILL_MIN_WIDTH))
                                    .max_w(px(PILL_MAX_WIDTH))
                                    .h(px(PILL_HEIGHT))
                                    .px(px(8.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .rounded(px(9.0))
                                    .border_1()
                                    .border_color(crate::theme::hairline(0.07))
                                    .bg(crate::theme::ink(0.03))
                                    .hover(|style| style.bg(crate::theme::ink(0.05)))
                                    .cursor_pointer()
                                    .text_size(crate::typography::ui_rems(12.0))
                                    .on_click(cx.listener(move |strip, _, _, cx| {
                                        strip.open(&pill_agent, cx)
                                    }))
                                    .child(crate::loaders::gradient_spinner(
                                        "running-subagent-spinner",
                                        &theme,
                                        2.0,
                                        view,
                                        cx,
                                    ))
                                    .child(
                                        div()
                                            .flex_shrink(1.0)
                                            .min_w_0()
                                            .max_w(px(TITLE_MAX_WIDTH))
                                            .truncate()
                                            .text_color(theme.text)
                                            .child(agent.title.clone()),
                                    )
                                    .when_some(
                                        agent.tail.clone().filter(|_| show_tails),
                                        |pill, tail: SharedString| {
                                            pill.child(
                                                div()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .truncate()
                                                    .text_color(theme.text_muted)
                                                    .child(tail),
                                            )
                                        },
                                    )
                            }),
                    )
                    .when(running.len() > MAX_PILLS, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .h(px(PILL_HEIGHT))
                                .px(px(8.0))
                                .flex()
                                .items_center()
                                .rounded(px(9.0))
                                .border_1()
                                .border_color(crate::theme::hairline(0.07))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(format!(
                                    "+{} more",
                                    running.len() - MAX_PILLS
                                ))),
                        )
                    }),
            )
            .into_any_element()
    }
}

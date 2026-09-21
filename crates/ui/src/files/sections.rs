//! The explorer's footer: two collapsible sections docked under the file
//! tree — **Subagents** (the spawn chips of the active chat's transcript,
//! with their live status) and **Chats** (the side chats hanging off the
//! active chat: forks, and chats an agent spawned through the Zeron MCP
//! server). Rows borrow the left sidebar's compact session row — 29px, status
//! glyph, title, time — minus the harness, project and device icons, which
//! say nothing here (every row shares the parent's context). Clicking a row
//! opens it in the right pane's surface host; the section chrome animates
//! with the same collapse motion as the sidebar's disclosures.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, EntityId, MouseButton, SharedString, div,
    prelude::*, px,
};
use zeron_doc::{MessagePart, SubagentStatus};
use zeron_proto::{Chat, ChatIndicator};

use crate::icons::{self, icon};
use crate::state::AppState;
use crate::theme::Theme;
use crate::{loaders, motion};

use super::{FilesEvent, FilesSurface};

const SECTION_HEADER_HEIGHT: f32 = 28.0;
const SECTION_BODY_INSET: f32 = 4.0;
const ROW_HEIGHT: f32 = 29.0;
const ROW_GAP: f32 = 2.0;
const EMPTY_ROW_HEIGHT: f32 = 24.0;
/// Rows an open section shows before it scrolls, so a busy orchestrator
/// never pushes the tree out of its own pane.
const MAX_VISIBLE_ROWS: usize = 6;
const TWEEN_GRACE: std::time::Duration = std::time::Duration::from_millis(120);

/// Which footer section a motion or toggle addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Section {
    Subagents,
    Chats,
}

impl Section {
    fn key(self) -> &'static str {
        match self {
            Section::Subagents => "subagents",
            Section::Chats => "chats",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Section::Subagents => "Subagents",
            Section::Chats => "Chats",
        }
    }

    fn empty_copy(self) -> &'static str {
        match self {
            Section::Subagents => "No subagents yet",
            Section::Chats => "No side chats yet",
        }
    }
}

/// One in-flight open/close of a section body (the sidebar's
/// `SidebarDisclosureMotion`, kept local so the explorer owns its own
/// epochs). A re-toggle mid-flight picks up from the painted height.
#[derive(Debug, Clone, Copy)]
struct DisclosureMotion {
    epoch: u64,
    from: f32,
    to: f32,
    started: std::time::Instant,
}

impl DisclosureMotion {
    fn current(self) -> f32 {
        let total = motion::COLLAPSE.total().as_secs_f32();
        let raw = if total > 0.0 {
            self.started.elapsed().as_secs_f32() / total
        } else {
            1.0
        };
        motion::lerp(self.from, self.to, motion::COLLAPSE.progress(raw))
    }

    fn animating(self) -> bool {
        self.started.elapsed() < motion::COLLAPSE.total() + TWEEN_GRACE
    }
}

/// Footer state on the explorer surface.
#[derive(Debug)]
pub(super) struct ExplorerSections {
    open: HashMap<Section, bool>,
    motion: HashMap<Section, DisclosureMotion>,
    /// Hash of what the footer would draw, so the state observer only
    /// re-renders the explorer when a section's contents actually changed —
    /// not on every streamed transcript delta.
    fingerprint: u64,
}

impl Default for ExplorerSections {
    fn default() -> Self {
        Self {
            open: [(Section::Subagents, true), (Section::Chats, true)]
                .into_iter()
                .collect(),
            motion: HashMap::new(),
            fingerprint: 0,
        }
    }
}

impl ExplorerSections {
    pub(super) fn is_open(&self, section: Section) -> bool {
        self.open.get(&section).copied().unwrap_or(true)
    }

    fn toggle(&mut self, section: Section, resting: f32, target: f32) {
        let previous = self.motion.get(&section).copied();
        let from = previous
            .filter(|m| m.animating())
            .map(DisclosureMotion::current)
            .unwrap_or(resting);
        let epoch = previous.map_or(1, |m| m.epoch + 1);
        self.motion.insert(
            section,
            DisclosureMotion {
                epoch,
                from,
                to: target,
                started: std::time::Instant::now(),
            },
        );
        let open = self.is_open(section);
        self.open.insert(section, !open);
    }

    fn live_motion(&self, section: Section) -> Option<DisclosureMotion> {
        self.motion.get(&section).copied().filter(|m| m.animating())
    }
}

/// A spawn chip of the active transcript, as the footer lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubagentRow {
    pub doc_id: String,
    pub title: SharedString,
    pub status: Option<SubagentStatus>,
}

impl SubagentRow {
    /// A settled subagent is frozen: the tab reads its blob first.
    pub fn frozen(&self) -> bool {
        matches!(
            self.status,
            Some(SubagentStatus::Done) | Some(SubagentStatus::Failed)
        )
    }

    fn indicator(&self) -> ChatIndicator {
        match self.status {
            Some(SubagentStatus::Running) => ChatIndicator::Working,
            Some(SubagentStatus::Done) => ChatIndicator::Completed,
            Some(SubagentStatus::Failed) => ChatIndicator::Errored,
            None => ChatIndicator::Idle,
        }
    }
}

/// The active chat's subagents, in spawn order, one row per subagent doc.
/// Only genuine spawn chips with a stamped doc ref qualify — the chip IS the
/// index (there is no listing endpoint), and a stray ref on a non-Agent tool
/// must not surface as a phantom subagent.
pub(super) fn subagent_rows(state: &AppState, chat_id: &str) -> Vec<SubagentRow> {
    if state.selected_chat.as_deref() != Some(chat_id) {
        return Vec::new();
    }
    let mut rows: Vec<SubagentRow> = Vec::new();
    for entry in &state.transcript {
        for part in &entry.parts {
            let MessagePart::Tool {
                call,
                subagent_ref: Some(doc_id),
                subagent_status,
                ..
            } = part
            else {
                continue;
            };
            if !call.is_subagent_spawn() {
                continue;
            }
            let row = SubagentRow {
                doc_id: doc_id.clone(),
                title: crate::transcript::subagent_tab_title(call),
                status: *subagent_status,
            };
            match rows.iter_mut().find(|r| r.doc_id == row.doc_id) {
                // A reopened (steered) subagent updates its row in place.
                Some(existing) => *existing = row,
                None => rows.push(row),
            }
        }
    }
    rows
}

/// A side chat of the active chat, as the footer lists it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChildChatRow {
    pub chat_id: String,
    pub title: SharedString,
    pub status: ChatIndicator,
    pub time_ago: SharedString,
    activity: DateTime<Utc>,
}

/// The live (unarchived) children of `chat_id`, most recent activity first —
/// the same order the sidebar's Sessions list keeps.
pub(super) fn child_chat_rows(
    state: &AppState,
    chat_id: &str,
    now: DateTime<Utc>,
) -> Vec<ChildChatRow> {
    let mut rows: Vec<ChildChatRow> = state
        .chats
        .iter()
        .filter(|chat| !chat.archived && chat.parent_chat_id.as_deref() == Some(chat_id))
        .map(|chat| {
            let activity = chat.last_message_at.unwrap_or(chat.created_at);
            ChildChatRow {
                chat_id: chat.id.clone(),
                title: child_chat_title(chat).into(),
                status: state.display_status_for(chat, now),
                time_ago: zeron_proto::view::format_time_ago(activity, now).into(),
                activity,
            }
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.activity));
    rows
}

/// A side chat titles itself on its first turn; until then the preview or a
/// placeholder stands in.
pub(super) fn child_chat_title(chat: &Chat) -> String {
    chat.title
        .clone()
        .or_else(|| chat.last_message_preview.clone())
        .unwrap_or_else(|| "New side chat".into())
}

/// What the footer would draw for `chat_id`, hashed. Cheap enough to run on
/// every state notification.
pub(super) fn fingerprint(state: &AppState, chat_id: &str, now: DateTime<Utc>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for row in subagent_rows(state, chat_id) {
        row.doc_id.hash(&mut hasher);
        row.title.as_ref().hash(&mut hasher);
        (row.status.map(|s| s as u8)).hash(&mut hasher);
    }
    0xC0FFEEu64.hash(&mut hasher);
    for row in child_chat_rows(state, chat_id, now) {
        row.chat_id.hash(&mut hasher);
        row.title.as_ref().hash(&mut hasher);
        (row.status as u8).hash(&mut hasher);
        row.time_ago.as_ref().hash(&mut hasher);
    }
    hasher.finish()
}

fn body_height(rows: usize) -> f32 {
    if rows == 0 {
        return SECTION_BODY_INSET + EMPTY_ROW_HEIGHT;
    }
    let shown = rows.min(MAX_VISIBLE_ROWS);
    SECTION_BODY_INSET + shown as f32 * ROW_HEIGHT + shown.saturating_sub(1) as f32 * ROW_GAP
}

impl FilesSurface {
    /// Re-render only when the footer's contents changed.
    pub(super) fn refresh_sections(&mut self, cx: &mut Context<Self>) {
        let fingerprint = fingerprint(self.state.read(cx), &self.chat_id, Utc::now());
        if fingerprint != self.sections.fingerprint {
            self.sections.fingerprint = fingerprint;
            cx.notify();
        }
    }

    pub(super) fn render_sections(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let now = Utc::now();
        let (subagents, chats) = {
            let state = self.state.read(cx);
            (
                subagent_rows(state, &self.chat_id),
                child_chat_rows(state, &self.chat_id, now),
            )
        };
        self.sections.fingerprint = fingerprint(self.state.read(cx), &self.chat_id, now);
        let view = cx.entity_id();
        let subagent_body = self.render_subagent_rows(&subagents, view, theme, cx);
        let chat_body = self.render_chat_rows(&chats, theme, cx);
        div()
            .id("files-sections")
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .px(px(6.0))
            .pt(px(4.0))
            .pb(px(6.0))
            .child(self.render_section(
                Section::Subagents,
                subagents.len(),
                None,
                subagent_body,
                theme,
                cx,
            ))
            .child(self.render_section(
                Section::Chats,
                chats.len(),
                Some(self.render_new_chat_button(theme, cx)),
                chat_body,
                theme,
                cx,
            ))
            .into_any_element()
    }

    /// The "+" on the Chats header: a fresh side chat of the active chat,
    /// opened in the right pane ready for its first message.
    fn render_new_chat_button(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("files-sections-new-chat")
            .role(gpui::Role::Button)
            .aria_label("New side chat")
            .flex_none()
            .size(px(20.0))
            .rounded(px(5.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .text_color(theme.text_muted.opacity(0.6))
            .hover(|s| s.bg(crate::theme::wash(0.09)).text_color(theme.text_muted))
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(FilesEvent::NewChildChat);
            }))
            .child(icon(icons::PLUS).size(px(12.0)))
            .into_any_element()
    }

    fn render_section(
        &mut self,
        section: Section,
        count: usize,
        action: Option<AnyElement>,
        body: AnyElement,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.sections.is_open(section);
        let full_height = body_height(count);
        // The collapsed header carries the count; open, the rows speak.
        let label: SharedString = if open || count == 0 {
            section.label().into()
        } else {
            format!("{} ({count})", section.label()).into()
        };
        let header = div()
            .id(SharedString::from(format!(
                "files-section-{}",
                section.key()
            )))
            .role(gpui::Role::Button)
            .aria_label(SharedString::from(format!(
                "{} {}",
                if open { "Collapse" } else { "Expand" },
                section.label()
            )))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(SECTION_HEADER_HEIGHT))
            .px(px(Theme::SPACE_SM))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|s| s.bg(crate::theme::wash(0.04)))
            .on_click(cx.listener(move |this, _, _, cx| {
                // Open → close runs from the full body height to 0, and back.
                let was_open = this.sections.is_open(section);
                let (resting, target) = if was_open {
                    (full_height, 0.0)
                } else {
                    (0.0, full_height)
                };
                this.sections.toggle(section, resting, target);
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(label),
            )
            .children(action)
            .child(self.render_chevron(section, open, theme));
        div()
            .flex()
            .flex_col()
            .child(header)
            .child(self.render_disclosure_body(section, open, full_height, body))
            .into_any_element()
    }

    fn render_chevron(&self, section: Section, open: bool, theme: &Theme) -> AnyElement {
        let chevron = icon(icons::ALT_ARROW_RIGHT)
            .size(px(12.0))
            .text_color(theme.text_muted.opacity(0.5));
        let frame = div().flex_none().size(px(12.0));
        if let Some(tween) = self.sections.live_motion(section) {
            let denominator = tween.from.max(tween.to).max(1.0);
            let from = (tween.from / denominator).clamp(0.0, 1.0);
            let to = (tween.to / denominator).clamp(0.0, 1.0);
            frame
                .child(chevron.with_animation(
                    SharedString::from(format!(
                        "files-section-chevron-{}-{}",
                        section.key(),
                        tween.epoch
                    )),
                    collapse_animation(),
                    move |el, t| {
                        let reveal = motion::lerp(from, to, t);
                        el.with_transformation(gpui::Transformation::rotate(gpui::percentage(
                            reveal * 0.25,
                        )))
                    },
                ))
                .into_any_element()
        } else {
            let resting = if open { 0.25 } else { 0.0 };
            frame
                .child(
                    chevron.with_transformation(gpui::Transformation::rotate(gpui::percentage(
                        resting,
                    ))),
                )
                .into_any_element()
        }
    }

    fn render_disclosure_body(
        &self,
        section: Section,
        open: bool,
        full_height: f32,
        content: AnyElement,
    ) -> AnyElement {
        let target = if open { full_height } else { 0.0 };
        let frame = div().w_full().flex_none().overflow_hidden().child(content);
        let Some(tween) = self.sections.live_motion(section) else {
            return frame.h(px(target)).into_any_element();
        };
        let denominator = full_height.max(1.0);
        frame
            .with_animation(
                SharedString::from(format!(
                    "files-section-body-{}-{}",
                    section.key(),
                    tween.epoch
                )),
                collapse_animation(),
                move |el, t| {
                    let height = motion::lerp(tween.from, tween.to, t);
                    let reveal = (height / denominator).clamp(0.0, 1.0);
                    el.h(px(height))
                        .opacity(0.35 + 0.65 * reveal)
                        .relative()
                        .top(px(-3.0 * (1.0 - reveal)))
                },
            )
            .into_any_element()
    }

    fn render_subagent_rows(
        &self,
        rows: &[SubagentRow],
        view: EntityId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if rows.is_empty() {
            return empty_row(Section::Subagents, theme);
        }
        let mut list = row_list("files-subagent-rows");
        for row in rows {
            let glyph = status_glyph(
                format!("files-subagent-{}", row.doc_id),
                row.indicator(),
                view,
                theme,
                cx,
            );
            let open = row.clone();
            list = list.child(
                compact_row(format!("files-subagent-{}", row.doc_id), theme)
                    .aria_label(SharedString::from(format!("Open subagent {}", row.title)))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(FilesEvent::OpenSubagent {
                            doc_id: open.doc_id.clone(),
                            title: open.title.to_string(),
                            frozen: open.frozen(),
                        });
                    }))
                    .child(glyph)
                    .child(row_title(row.title.clone())),
            );
        }
        list.into_any_element()
    }

    fn render_chat_rows(
        &self,
        rows: &[ChildChatRow],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if rows.is_empty() {
            return empty_row(Section::Chats, theme);
        }
        let view = cx.entity_id();
        let mut list = row_list("files-chat-rows");
        for row in rows {
            let glyph = status_glyph(
                format!("files-chat-{}", row.chat_id),
                row.status,
                view,
                theme,
                cx,
            );
            let open_id = row.chat_id.clone();
            list = list.child(
                compact_row(format!("files-chat-{}", row.chat_id), theme)
                    .aria_label(SharedString::from(format!("Open side chat {}", row.title)))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(FilesEvent::OpenChildChat(open_id.clone()));
                    }))
                    .child(glyph)
                    .child(row_title(row.title.clone()))
                    .child(
                        div()
                            .flex_none()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted.opacity(0.5))
                            .child(row.time_ago.clone()),
                    ),
            );
        }
        list.into_any_element()
    }
}

fn collapse_animation() -> Animation {
    motion::COLLAPSE.animation()
}

/// The scrolling column an open section's rows live in, capped at
/// [`MAX_VISIBLE_ROWS`] so the footer never swallows the tree.
fn row_list(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(px(ROW_GAP))
        .pt(px(SECTION_BODY_INSET))
        .max_h(px(body_height(MAX_VISIBLE_ROWS)))
        .overflow_y_scroll()
}

fn empty_row(section: Section, theme: &Theme) -> AnyElement {
    div()
        .h(px(EMPTY_ROW_HEIGHT))
        .mt(px(SECTION_BODY_INSET))
        .px(px(Theme::SPACE_SM))
        .flex()
        .items_center()
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_muted.opacity(0.4))
        .child(section.empty_copy())
        .into_any_element()
}

/// The sidebar's compact session row, stripped to status + title (+ time):
/// 29px, 8px radius, the glass hover wash, 13px title on a 17px line.
fn compact_row(id: String, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .role(gpui::Role::Button)
        .flex_none()
        .h(px(ROW_HEIGHT))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .rounded(px(8.0))
        .px(px(Theme::SPACE_SM))
        .cursor_pointer()
        .text_color(theme.text.opacity(0.8))
        .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
}

fn row_title(title: SharedString) -> gpui::Div {
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .text_size(crate::typography::ui_rems(13.0))
        .line_height(px(17.0))
        .child(title)
}

/// The compact row's 13px status slot: Working animates the glyph spinner,
/// Completed wears the check, the rest a 6px dot in the status color.
fn status_glyph(
    key: String,
    status: ChatIndicator,
    view: EntityId,
    theme: &Theme,
    cx: &mut gpui::App,
) -> AnyElement {
    let color = crate::shell::spaces::status_dot_color(status, theme);
    let glyph: AnyElement = match status {
        ChatIndicator::Completed => icon(icons::CHECK)
            .size(px(11.0))
            .flex_none()
            .text_color(color)
            .into_any_element(),
        ChatIndicator::Working => {
            loaders::mini_glyph_spinner(format!("{key}-working"), 2.0, theme.glyph, view, cx)
                .into_any_element()
        }
        _ => div()
            .size(px(6.0))
            .flex_none()
            .rounded_full()
            .bg(color)
            .into_any_element(),
    };
    div()
        .flex_none()
        .size(px(13.0))
        .flex()
        .items_center()
        .justify_center()
        .child(glyph)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_doc::{MessageRole, MessageStatus, SessionMessageEntry};
    use zeron_proto::ToolCall;

    fn chat(id: &str, parent: Option<&str>, minutes_ago: i64) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "deviceId": "dev",
            "archived": false,
            "createdAt": Utc::now() - chrono::Duration::minutes(minutes_ago),
            "parentChatId": parent,
        }))
        .unwrap()
    }

    fn spawn(
        id: &str,
        name: &str,
        doc: Option<&str>,
        status: Option<SubagentStatus>,
    ) -> MessagePart {
        MessagePart::Tool {
            id: id.into(),
            call: ToolCall::Unknown {
                name: name.into(),
                input: Some(serde_json::json!({ "description": "verify the marker pipeline" })),
            },
            is_error: false,
            resolved: true,
            output: None,
            diff: None,
            output_ref: None,
            output_bytes: None,
            diff_ref: None,
            diff_stats: None,
            subagent_ref: doc.map(str::to_owned),
            subagent_status: status,
            subagent_tail: None,
        }
    }

    fn entry(parts: Vec<MessagePart>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: "e1".into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "dev".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn subagent_rows_list_only_stamped_spawn_chips_of_the_selected_chat() {
        let mut state = AppState::new();
        state.selected_chat = Some("main".into());
        state.transcript = vec![entry(vec![
            spawn(
                "t1",
                "Agent: verify",
                Some("main--sub--t1"),
                Some(SubagentStatus::Running),
            ),
            // No doc ref yet: the engine stamps it asynchronously.
            spawn("t2", "Agent: later", None, None),
            // A stray ref on a non-spawn tool never surfaces.
            spawn(
                "t3",
                "Read",
                Some("main--sub--t3"),
                Some(SubagentStatus::Done),
            ),
            spawn(
                "t4",
                "Agent: done",
                Some("main--sub--t4"),
                Some(SubagentStatus::Done),
            ),
        ])];
        let rows = subagent_rows(&state, "main");
        assert_eq!(
            rows.iter().map(|r| r.doc_id.as_str()).collect::<Vec<_>>(),
            ["main--sub--t1", "main--sub--t4"]
        );
        // The bare task, genus stripped — the same title the tab wears.
        assert_eq!(rows[0].title.as_ref(), "verify");
        assert!(!rows[0].frozen());
        assert!(rows[1].frozen());
        // Another chat's explorer sees nothing of this transcript.
        assert!(subagent_rows(&state, "other").is_empty());
    }

    #[test]
    fn child_chat_rows_are_live_children_newest_first() {
        let mut state = AppState::new();
        let mut archived = chat("old", Some("main"), 1);
        archived.archived = true;
        let mut titled = chat("b", Some("main"), 30);
        titled.title = Some("Investigate caching".into());
        state.apply_chats(vec![
            chat("main", None, 60),
            chat("a", Some("main"), 5),
            titled,
            chat("unrelated", Some("elsewhere"), 2),
            archived,
        ]);
        let rows = child_chat_rows(&state, "main", Utc::now());
        assert_eq!(
            rows.iter().map(|r| r.chat_id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(rows[0].title.as_ref(), "New side chat");
        assert_eq!(rows[1].title.as_ref(), "Investigate caching");
        assert_eq!(rows[0].status, ChatIndicator::Idle);
    }

    #[test]
    fn fingerprint_tracks_membership_and_status() {
        let mut state = AppState::new();
        let now = Utc::now();
        state.apply_chats(vec![chat("main", None, 60)]);
        let empty = fingerprint(&state, "main", now);
        state.apply_chats(vec![chat("main", None, 60), chat("a", Some("main"), 5)]);
        let one = fingerprint(&state, "main", now);
        assert_ne!(empty, one);
        assert_eq!(one, fingerprint(&state, "main", now));
    }

    #[test]
    fn body_height_caps_at_the_visible_row_budget() {
        assert_eq!(body_height(0), SECTION_BODY_INSET + EMPTY_ROW_HEIGHT);
        assert_eq!(body_height(1), SECTION_BODY_INSET + ROW_HEIGHT);
        assert_eq!(
            body_height(MAX_VISIBLE_ROWS),
            body_height(MAX_VISIBLE_ROWS + 10)
        );
    }
}

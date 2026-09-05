//! Reusable controls: buttons, pills, cards, rows, chrome.
//!
//! Nothing here knows what screen it is on. If a control needs to know, it
//! belongs in `view.rs` instead.

use super::theme::*;
use gpui::{div, prelude::*, px, rgb, Animation, AnimationExt, FontWeight, SharedString};

/// Small square affordance on a toast: collapse, expand, or dismiss.
pub(crate) fn toast_toggle(id: &'static str, glyph: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(px(20.0))
        .h(px(20.0))
        .rounded_md()
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
        .child(glyph)
}

pub(crate) fn avatar(
    image: Option<std::sync::Arc<gpui::RenderImage>>,
    name: &str,
    size: f32,
) -> gpui::Div {
    let initial = name
        .chars()
        .next()
        .map(|ch| ch.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".into());
    let content = match image {
        Some(image) => appear(
            "avatar-in",
            motion::ENTER,
            gpui::img(image)
                .w(px(size - 2.0))
                .h(px(size - 2.0))
                .rounded_full()
                .overflow_hidden()
                .object_fit(gpui::ObjectFit::Cover),
        )
        .into_any_element(),
        None => label(initial, TEXT)
            .font_family("Bahnschrift")
            .text_size(px(size * 0.42))
            .font_weight(FontWeight::SEMIBOLD)
            .into_any_element(),
    };
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(SURFACE_HOVER))
        .border_1()
        .border_color(rgb(BORDER))
        .child(content)
}

/// A raised surface with a hairline border. The border does most of the work:
/// on a dark UI, background alone reads as mush.
pub(crate) fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
}

/// The one action a screen exists for.
///
/// `arrow` puts a trailing chevron on the far edge and pushes the label to the
/// leading one, which is how the boards draw a button that moves you forward.
/// Actions that merely acknowledge something - waiting on a browser, signing
/// in - stay centred and arrowless, so the arrow keeps meaning "next".
pub(crate) fn primary(
    id: &'static str,
    text: impl Into<SharedString>,
    arrow: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .w_full()
        .h(px(44.0))
        .px_4()
        .rounded_md()
        .bg(rgb(ORANGE))
        .text_color(rgb(INK))
        .font_family("Bahnschrift")
        .text_size(px(13.0))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(ORANGE_HOT)))
        // Press feedback: without it, a click feels like nothing happened
        // until the screen changes.
        .active(|s| s.bg(rgb(ORANGE_DIM)))
        .map(|element| {
            if arrow {
                element
                    .justify_between()
                    .child(text.into())
                    .child(div().text_size(px(14.0)).child("\u{2192}"))
            } else {
                element.justify_center().child(text.into())
            }
        })
}

pub(crate) fn secondary(
    id: &'static str,
    text: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .h(px(44.0))
        .px_4()
        .rounded_md()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .text_color(rgb(TEXT))
        .font_family("Bahnschrift")
        .text_size(px(13.0))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(SURFACE_HOVER)).border_color(rgb(BORDER_HOVER)))
        .active(|s| s.bg(rgb(BG)))
        .child(text.into())
}

/// A named exit: Sign out, Refresh, Back, Done.
///
/// Orange on nothing. These are the moves the boards draw in the accent
/// because they change where you are rather than what you have configured,
/// and there is at most one of them visible per screen - which is the only
/// reason spending the accent on them does not dilute it.
pub(crate) fn ghost(id: &'static str, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .h(px(30.0))
        .px_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(ORANGE_DIM))
        .text_xs()
        .text_color(rgb(ORANGE))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(ORANGE_WASH)).border_color(rgb(ORANGE)))
        .active(|s| s.bg(rgb(BG)))
        .child(text.into())
}

/// A secondary action inside a settings row: Check now, Open folder.
///
/// Bordered rather than bare text. As plain FAINT text it was styled
/// identically to the caption beside it, so on the Diagnostics row "Logs from
/// your recent sessions" and "Open folder" were indistinguishable until hover -
/// one inert, one the only control in the row.
///
/// Neutral rather than orange, unlike `ghost`: several of these can be on
/// screen at once, and the board's own first rule is that orange is an accent
/// and not the dominant colour.
pub(crate) fn quiet(id: &'static str, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        // Without this a long description in the same row squeezes the action
        // until its text is clipped, which is how "Open folder" became
        // "Open folde".
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(SURFACE_HOVER))
                .border_color(rgb(ORANGE_DIM))
                .text_color(rgb(TEXT))
        })
        .child(text.into())
}

/// A settings row: a title with supporting text, and an action on the right.
///
/// Shared rather than hand-assembled per row, because hand-assembling is how
/// the Updates and Diagnostics rows drifted apart: one had `min_w(0)` on its
/// text column and the other did not, so only one of them clipped.
pub(crate) fn setting_row(
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    action: gpui::AnyElement,
) -> gpui::Div {
    card()
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                // Lets the column shrink below its content, so the description
                // wraps instead of pushing the action off the card.
                .min_w(px(0.0))
                .child(heading(title, 12.0))
                .child(label(detail, MUTED).text_xs()),
        )
        .child(action)
}

/// A settings row whose control is a strip of pills below the title.
///
/// The title row carries an optional readout - what the abstract label actually
/// resolves to - in the machine voice, matching the streaming header. Derived
/// numbers belong there rather than welded into the prose below, where changing
/// resolution could push the bitrate string past a wrap point and so change the
/// height of a card two rows further down.
///
/// The detail line describes the *selected* option rather than the setting as a
/// whole. A single line covering every option has to be written vaguely enough
/// to fit them all, which is how "Auto follows the refresh rate" ended up
/// reading as a claim about the whole setting instead of about Auto.
pub(crate) fn setting_choice(
    title: impl Into<SharedString>,
    readout: Option<SharedString>,
    pills: Vec<gpui::AnyElement>,
    detail: impl Into<SharedString>,
) -> gpui::Div {
    let title = title.into();
    let detail = detail.into();
    // The animation id carries the text, so picking a different option replays
    // the fade instead of swapping the line instantly. Prefixed with the title
    // because two cards could otherwise share a detail string and an id.
    let key = SharedString::from(format!("{title}/{detail}"));

    let heading = match readout {
        Some(readout) => div()
            .flex()
            .items_center()
            .gap_2()
            .child(heading(title.clone(), 12.0))
            .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
            .child(appear(
                SharedString::from(format!("{title}={readout}")),
                motion::QUICK,
                micro(readout, MUTED),
            )),
        None => div().flex().child(heading(title, 12.0)),
    };

    card()
        .flex_shrink_0()
        .gap_2()
        .child(heading)
        .child(div().flex().flex_wrap().gap_1p5().children(pills))
        .child(
            // One line of reserved height, so a longer string in some future
            // state cannot reflow the list while a fade is still running.
            div()
                .min_h(px(16.0))
                .child(appear(key, motion::QUICK, label(detail, MUTED).text_xs())),
        )
}

/// The contents of an expanded section.
///
/// Each card fades on its own, all starting together but taking progressively
/// longer, so they arrive in order. That is a stagger without a delay
/// primitive, which GPUI does not have. Movement is deliberately not animated:
/// every offset GPUI can express is a margin or padding, so a slide would
/// reflow the scroll container on every frame.
pub(crate) fn section_body(id: &str, children: Vec<gpui::AnyElement>) -> impl IntoElement {
    let animated: Vec<gpui::AnyElement> = children
        .into_iter()
        .enumerate()
        .map(|(index, child)| {
            appear(
                format!("{id}-{index}"),
                motion::ENTER + motion::STAGGER * index as u32,
                div().flex_shrink_0().child(child),
            )
            .into_any_element()
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap_3()
        .children(animated)
}

/// A collapsible section heading: a scanline, the title, a rule, an optional
/// readout, and a chevron showing state.
///
/// The marker is a bar rather than a glyph. Nothing that sits in the same row
/// as its own text label can disambiguate anything, and the three glyphs this
/// replaces were not even a family - a concentric ring, nested squares and a
/// striped square, one row apart. A 12x2 bar is a scanline, which is the atom
/// the mark itself is built from, identical across every section so it makes no
/// claim it cannot keep, and it cannot fall back to tofu.
///
/// Typography sits on the row itself rather than on each child, so the whole
/// heading brightens as one on hover; `micro` would pin each child's colour and
/// defeat that.
pub(crate) fn section_header(
    id: &'static str,
    title: &'static str,
    readout: Option<&'static str>,
    open: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        // 26px of target instead of 18: this row exists to be clicked.
        .py_2()
        .cursor_pointer()
        .font_family("Cascadia Mono")
        .text_size(px(10.0))
        .font_weight(FontWeight::MEDIUM)
        // Full ORANGE at rest. A section heading is the primary structure of
        // the screen, not chrome; ORANGE_DIM on this background is 2.4:1, which
        // made the top of the hierarchy the second-dimmest thing on it.
        .text_color(rgb(ORANGE))
        .hover(|style| style.text_color(rgb(ORANGE_HOT)))
        .child(accent_rule(12.0))
        .child(div().child(title))
        // The same rule the streaming header uses between its label and its
        // readout, so headings across the app read as one pattern. It also
        // pushes the tail to the far edge and makes the whole strip a target.
        .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
        .children(readout.map(|text| div().text_color(rgb(MUTED)).child(text)))
        // Bigger than the label, because this is the only glyph in the row that
        // encodes anything: whether the section is open.
        .child(
            div()
                .text_size(px(12.0))
                .child(if open { "\u{25BE}" } else { "\u{25B8}" }),
        )
}

pub(crate) fn update_action(
    id: &'static str,
    text: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(104.0))
        .h(px(36.0))
        .px_3()
        .rounded_md()
        .bg(rgb(ORANGE))
        .text_color(rgb(INK))
        .font_family("Bahnschrift")
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|style| style.bg(rgb(ORANGE_HOT)))
        .active(|style| style.bg(rgb(ORANGE_DIM)))
        .child(text.into())
}

pub(crate) fn option_pill(
    id: SharedString,
    text: impl Into<SharedString>,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .border_1()
        .border_color(rgb(if active { ORANGE } else { BORDER }))
        .bg(rgb(if active { ORANGE } else { SURFACE }))
        .text_color(rgb(if active { INK } else { MUTED }))
        .when(active, |element| element.font_weight(FontWeight::SEMIBOLD))
        .when(!active, |element| {
            element.hover(|style| {
                style
                    .bg(rgb(SURFACE_HOVER))
                    .border_color(rgb(ORANGE_DIM))
                    .text_color(rgb(TEXT))
            })
        })
        .child(text.into())
}

/// A concentric broadcast mark: a lit centre with two rings around it.
///
/// Built from three divs rather than set as a glyph. The titlebar can lean on
/// Segoe UI Symbol because its three glyphs are ancient and universal; a
/// broadcast icon is neither, and a missing one falls back to a tofu box in
/// the most prominent control on the screen.
pub(crate) fn broadcast_mark(size: f32, color: u32) -> gpui::Div {
    let ring = |diameter: f32, alpha_color: u32| {
        div()
            .absolute()
            .w(px(diameter))
            .h(px(diameter))
            .left(px((size - diameter) / 2.0))
            .top(px((size - diameter) / 2.0))
            .rounded_full()
            .border_1()
            .border_color(rgb(alpha_color))
    };
    div()
        .relative()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .child(ring(size, color))
        .child(ring(size * 0.62, color))
        .child(
            div()
                .absolute()
                .w(px(size * 0.24))
                .h(px(size * 0.24))
                .left(px(size * 0.38))
                .top(px(size * 0.38))
                .rounded_full()
                .bg(rgb(color)),
        )
}

/// Two figures: somebody else's stream.
///
/// A head and a pair of shoulders each, rather than two rings. Two circles
/// side by side is what a pair of goggles looks like; the shoulders are what
/// make it a person.
pub(crate) fn people_mark(size: f32, color: u32) -> gpui::Div {
    let figure = |x: f32, scale: f32| {
        let head = size * 0.26 * scale;
        let shoulders = size * 0.46 * scale;
        div()
            .absolute()
            .left(px(x))
            .bottom(px(size * 0.18))
            .w(px(shoulders))
            .h(px(size * 0.6 * scale))
            .child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .left(px((shoulders - head) / 2.0))
                    .w(px(head))
                    .h(px(head))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(color)),
            )
            .child(
                // Only the top corners are rounded, so it reads as a torso
                // continuing past the frame rather than a floating pill.
                div()
                    .absolute()
                    .bottom(px(0.0))
                    .left(px(0.0))
                    .w(px(shoulders))
                    .h(px(size * 0.24 * scale))
                    .rounded_t(px(shoulders / 2.0))
                    .border_1()
                    .border_color(rgb(color)),
            )
    };
    div()
        .relative()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        // The smaller figure first, so the nearer one overlaps it.
        .child(figure(size * 0.42, 0.82))
        .child(figure(0.0, 1.0))
}

/// The rounded well a mark sits in on an action card.
pub(crate) fn icon_tile(mark: gpui::Div, border: u32) -> gpui::Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(px(46.0))
        .h(px(46.0))
        .rounded_md()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(border))
        .child(mark)
}

/// A whole route as one target: mark, title, what it does, and an arrow.
///
/// This replaces a stack of bare buttons whose labels had to carry all the
/// meaning on their own. The supporting line is the point - "Start streaming"
/// and "Join a stream" are indistinguishable to somebody opening the app for
/// the first time, and a caption costs nothing but a row of height.
///
/// `accent` marks the one the screen is actually for. Exactly one card per
/// screen may set it, or the accent stops meaning anything.
pub(crate) fn action_card(
    id: &'static str,
    mark: gpui::Div,
    title: &'static str,
    detail: &'static str,
    accent: bool,
) -> gpui::Stateful<gpui::Div> {
    let edge = if accent { ORANGE_DIM } else { BORDER };
    let group = SharedString::from(format!("action-{id}"));
    div()
        .id(id)
        .group(group.clone())
        .flex()
        .items_center()
        .gap_3()
        .w_full()
        .flex_shrink_0()
        .p_3()
        .rounded_md()
        .bg(rgb(if accent { ORANGE_WASH } else { SURFACE }))
        .border_1()
        .border_color(rgb(edge))
        .cursor_pointer()
        .hover(|style| style.border_color(rgb(if accent { ORANGE } else { BORDER_HOVER })))
        .active(|style| style.bg(rgb(BG)))
        .child(icon_tile(mark, edge))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    label(title, if accent { ORANGE } else { TEXT })
                        .font_family("Bahnschrift")
                        .text_size(px(14.0))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .child(label(detail, MUTED).text_xs()),
        )
        .child(
            // Slides a couple of pixels on hover. The only motion in the card,
            // so it reads as the card acknowledging the cursor rather than as
            // decoration.
            div()
                .flex_shrink_0()
                .text_size(px(15.0))
                .text_color(rgb(if accent { ORANGE } else { FAINT }))
                .pr_1()
                .group_hover(group, |style| style.pr_0().text_color(rgb(ORANGE)))
                .child("\u{2192}"),
        )
}

/// Who you are signed in as: portrait, a rule, and the name.
///
/// The rule is the boards' idea and a good one - it ties the portrait to the
/// text as one object, so the footer reads as a single identity rather than as
/// an image that happens to sit next to a caption.
pub(crate) fn identity(
    image: Option<std::sync::Arc<gpui::RenderImage>>,
    name: impl Into<SharedString>,
    caption: &'static str,
) -> gpui::Div {
    let name = name.into();
    div()
        .flex()
        .items_center()
        .gap_3()
        .min_w(px(0.0))
        .child(
            // The ring is a second element rather than a border on the
            // portrait: a border would eat into the image at this size, and
            // the portrait is already round and clipped.
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .w(px(40.0))
                .h(px(40.0))
                .rounded_full()
                .border_1()
                .border_color(rgb(ORANGE_DIM))
                .child(avatar(image, &name, 32.0)),
        )
        .child(div().w(px(2.0)).h(px(28.0)).flex_shrink_0().bg(rgb(ORANGE)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .min_w(px(0.0))
                .child(micro(caption, FAINT))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(rgb(TEXT))
                        .child(name),
                ),
        )
}

/// A small coloured dot, for status.
pub(crate) fn dot(color: u32) -> gpui::Div {
    div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(color))
}

/// The bottom edge of a scrolling region, faded into the background to say
/// there is more below. `None` when the region is at its end, or does not
/// scroll at all.
///
/// GPUI has no scrollbar, so `overflow_y_scroll` on its own gives a list no
/// edge: a picker showing four sources out of twelve looks exactly like one
/// showing four out of four, and the settings list simply ran out under the
/// version footer.
///
/// The condition lives here rather than at the call sites so the two regions
/// cannot drift apart on when they show an edge.
pub(crate) fn scroll_fade(scroll: &gpui::ScrollHandle) -> Option<gpui::Div> {
    // Offsets run negative as a region scrolls down, hence the absolute value.
    // `max_offset` is zero until the region has been laid out and for as long
    // as everything fits, which is exactly when there should be no edge. The
    // pixel of tolerance stops the fade flickering back on at the bottom.
    let travelled = scroll.offset().y.abs();
    let total = scroll.max_offset().height;
    if total <= px(0.0) || travelled >= total - px(1.0) {
        return None;
    }

    let base: gpui::Hsla = rgb(BG).into();
    Some(
        // Drawn over the content rather than beside it, so appearing and
        // disappearing never changes the width of the region and reflows it.
        div()
            .absolute()
            .bottom(px(0.0))
            .left(px(0.0))
            .w_full()
            .h(px(28.0))
            // Angle 0 points at the top, so the first stop is the bottom edge.
            // Both stops are the background colour and only the alpha moves,
            // which is what keeps the fade from greying the cards under it.
            .bg(gpui::linear_gradient(
                0.0,
                gpui::linear_color_stop(base, 0.0),
                gpui::linear_color_stop(base.opacity(0.0), 1.0),
            )),
    )
}

/// A dot that breathes, for "this is live right now".
pub(crate) fn live_dot(moving: bool) -> impl IntoElement {
    match live_dot_animation(moving) {
        Some(animation) => dot(SUCCESS)
            .with_animation(SharedString::from("live-pulse"), animation, |el, delta| {
                el.opacity(delta)
            })
            .into_any_element(),
        None => dot(SUCCESS).opacity(0.7).into_any_element(),
    }
}

fn live_dot_animation(moving: bool) -> Option<Animation> {
    // A repeating animation invalidates the entire screen, even though this
    // dot is tiny. Use the same active-window gate as the grid and logo aura.
    moving.then(|| {
        Animation::new(motion::BREATH)
            .repeat()
            .with_easing(gpui::pulsating_between(0.35, 1.0))
    })
}

/// The badge that appears on a picker preview under the cursor.
///
/// The boards draw a tick here. This is an arrow, because a tick means "this
/// one is selected" and there is no selected state in the picker - clicking a
/// card starts the stream outright. Same circle, honest verb.
pub(crate) fn go_badge() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(28.0))
        .h(px(28.0))
        .rounded_full()
        .bg(rgb(INK))
        .border_1()
        .border_color(rgb(ORANGE))
        .text_size(px(13.0))
        .text_color(rgb(ORANGE))
        .child("\u{2192}")
}

/// Square, unobtrusive control in the titlebar.
///
/// The hover colour is a parameter rather than something callers add
/// afterwards: GPUI panics if `.hover()` is applied twice to one element.
pub(crate) fn titlebar_button(
    id: &'static str,
    glyph: &'static str,
    hover_bg: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(38.0))
        .h(px(36.0))
        .rounded_md()
        .font_family("Segoe UI Symbol")
        .text_size(px(15.0))
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover_bg)).text_color(rgb(TEXT)))
        .child(glyph)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_live_indicators_do_not_schedule_a_repeating_animation() {
        // Grid and aura already stopped on blur, but this remaining animation
        // kept invalidating their entire containing screen in the background.
        assert!(live_dot_animation(false).is_none());
        assert!(live_dot_animation(true).is_some());
    }
}

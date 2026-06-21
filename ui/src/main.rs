//! logsnap-ui — a small, read-only desktop viewer over a logsnap session.
//!
//! Layout mirrors the user's mental model: a **sidebar** of snapshots (live
//! "uncommitted" plus every checkpoint), **tabs** per watched file, and a
//! virtualized **scrollview** of the selected slice's lines.
//!
//! All read logic is reused from the `logsnap` library (`resolve`/`region` for the
//! live slice, the `diff --in` byte-range re-read for checkpoints) — the CLI remains
//! the only writer of the session. Refresh is manual for now (a button); event-driven
//! updates via inotify can come later.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Root, Sizable, StyledExt, Theme, ThemeMode, VirtualListScrollHandle,
    button::Button,
    h_flex,
    scroll::{ScrollableElement, ScrollbarShow},
    tab::{Tab, TabBar},
    v_flex, v_virtual_list,
};
use logsnap::{Event, Fs, OsFs, State, load_state, region, resolve, short};

/// Height of one rendered log line, in pixels.
const ROW: f32 = 20.;

/// Which slice of a file is shown: the live pending lines, or one past checkpoint.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Snapshot {
    Uncommitted,
    Checkpoint(u32),
}

struct LogViewer {
    fs: OsFs,
    /// Loaded session, or `None` if there is no session on disk.
    state: Option<State>,
    /// A load error to surface instead of content (e.g. "no logsnap session").
    error: Option<SharedString>,
    selected_file: usize,
    snapshot: Snapshot,

    /// The currently displayed lines (rebuilt by `recompute`).
    lines: Vec<SharedString>,
    item_sizes: Rc<Vec<Size<Pixels>>>,
    /// A per-slice note (truncation/rotation/unavailable), shown above the content.
    note: Option<SharedString>,

    /// One scroll position per file, so each tab keeps its own place in the log.
    file_scrolls: Vec<VirtualListScrollHandle>,
    sidebar_scroll: ScrollHandle,
}

impl LogViewer {
    fn new() -> Self {
        let mut this = Self {
            fs: OsFs,
            state: None,
            error: None,
            selected_file: 0,
            snapshot: Snapshot::Uncommitted,
            lines: Vec::new(),
            item_sizes: Rc::new(Vec::new()),
            note: None,
            file_scrolls: Vec::new(),
            sidebar_scroll: ScrollHandle::new(),
        };
        this.reload();
        this
    }

    /// Re-read the session from disk and rebuild the current view. Called on start
    /// and from the refresh button — picks up CLI commits and freshly appended lines.
    fn reload(&mut self) {
        match load_state() {
            Ok((state, _path)) => {
                let n = state.files.len();
                if self.selected_file >= n {
                    self.selected_file = 0;
                }
                // Keep one scroll handle per file; preserve existing ones across reloads
                // (so a refresh doesn't reset the scroll position).
                self.file_scrolls
                    .resize_with(n, VirtualListScrollHandle::new);
                self.state = Some(state);
                self.error = None;
            }
            Err(e) => {
                self.state = None;
                self.error = Some(e.into());
            }
        }
        self.recompute();
    }

    /// Rebuild `lines`/`item_sizes`/`note` for the selected (file, snapshot).
    fn recompute(&mut self) {
        let (bytes, note) = match &self.state {
            Some(state) => slice_bytes(state, &self.fs, self.selected_file, self.snapshot),
            None => (Vec::new(), None),
        };
        self.note = note;
        self.lines = String::from_utf8_lossy(&bytes)
            .split_inclusive('\n')
            .map(|l| SharedString::from(l.strip_suffix('\n').unwrap_or(l).to_string()))
            .collect();
        // Width is ignored by the vertical virtual list (it measures the first row).
        self.item_sizes = Rc::new(vec![size(px(0.), px(ROW)); self.lines.len()]);
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = cx.theme().sidebar_accent;
        let accent_fg = cx.theme().sidebar_accent_foreground;

        // One snapshot entry: a full-width, edge-to-edge clickable bar. Spacing comes
        // from inner padding (not an outer gap), so there is no dead, non-clickable space
        // between entries — unlike gpui-component's `SidebarMenu`. `px_3` aligns the text
        // with the header/label; the highlight spans the full panel width (no rounding).
        let row = move |id: ElementId, label: SharedString, active: bool| {
            div()
                .id(id)
                .w_full()
                .px_3()
                .py_1()
                .text_sm()
                // Keep each entry a single line (ellipsis on overflow). Without this a long
                // label wraps, which collapses the flex column to the wrapped width and makes
                // every row's hover/active bar only as wide as that content.
                .truncate()
                .cursor_pointer()
                .when(!active, move |this| {
                    this.hover(move |s| s.bg(accent.opacity(0.5)))
                })
                .when(active, move |this| {
                    this.font_medium().bg(accent).text_color(accent_fg)
                })
                .child(label)
        };

        let mut list = v_flex().w_full().child(
            row(
                "snap-uncommitted".into(),
                "● uncommitted".into(),
                self.snapshot == Snapshot::Uncommitted,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.snapshot = Snapshot::Uncommitted;
                this.recompute();
                cx.notify();
            })),
        );
        if let Some(state) = &self.state {
            // Newest checkpoint first, like `logsnap list` reversed.
            for c in state.history.iter().rev() {
                let id = c.id;
                let when = c.created_at.clone().unwrap_or_default();
                let msg = c.message.clone().unwrap_or_default();
                let label = format!("#{id}  {when}  {msg}");
                list = list.child(
                    row(
                        SharedString::from(format!("snap-{id}")).into(),
                        label.into(),
                        self.snapshot == Snapshot::Checkpoint(id),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.snapshot = Snapshot::Checkpoint(id);
                        this.recompute();
                        cx.notify();
                    })),
                );
            }
        }

        // Custom sidebar panel: gpui-component's `Sidebar` only accepts `SidebarItem`
        // children (and its `SidebarMenu` forces a gap between items), so we build our
        // own to keep the snapshot list edge-to-edge clickable and scrollable.
        v_flex()
            .w(px(280.))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .items_center()
                    .child(div().font_bold().child("logsnap"))
                    .child(
                        Button::new("refresh")
                            .label("⟳")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("SNAPSHOTS"),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("snap-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .pb_2()
                            .child(list),
                    )
                    .vertical_scrollbar(&self.sidebar_scroll),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = TabBar::new("files")
            .selected_index(self.selected_file)
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.selected_file = *ix;
                this.recompute();
                cx.notify();
            }));
        if let Some(state) = &self.state {
            for f in &state.files {
                bar = bar.child(Tab::new().label(short(&f.path).to_string()));
            }
        }
        bar
    }

    fn render_content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;

        let body: AnyElement = if let Some(err) = &self.error {
            div()
                .p_4()
                .text_color(muted)
                .child(err.clone())
                .into_any_element()
        } else if self.lines.is_empty() {
            let msg = self
                .note
                .clone()
                .unwrap_or_else(|| "up to date — nothing here".into());
            div().p_4().text_color(muted).child(msg).into_any_element()
        } else {
            // Per-file scroll handle: switching tabs keeps each file's own position.
            // Safe to index — a non-empty `lines` means a valid selected file.
            let scroll = &self.file_scrolls[self.selected_file];
            let list = v_virtual_list(
                cx.entity().clone(),
                "log-lines",
                self.item_sizes.clone(),
                move |this, range, _, cx| {
                    let muted = cx.theme().muted_foreground;
                    range
                        .map(|ix| {
                            let line = this.lines.get(ix).cloned().unwrap_or_default();
                            h_flex()
                                .w_full()
                                .h(px(ROW))
                                .child(
                                    div()
                                        .w(px(56.))
                                        .flex_shrink_0()
                                        .pr_3()
                                        .text_right()
                                        .text_color(muted)
                                        .child(format!("{}", ix + 1)),
                                )
                                .child(div().flex_1().child(line))
                        })
                        .collect()
                },
            )
            .track_scroll(scroll)
            .font_family(cx.theme().mono_font_family.clone())
            .text_sm()
            .px_3()
            .py_2();

            div()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(list)
                .vertical_scrollbar(scroll)
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .when_some(
                self.note.clone().filter(|_| !self.lines.is_empty()),
                |this, note| {
                    this.child(div().px_3().py_1().text_xs().text_color(muted).child(note))
                },
            )
            .child(body)
    }
}

impl Render for LogViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_sidebar(cx))
            .child(
                v_flex()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_tabs(cx))
                    .child(self.render_content(cx)),
            )
    }
}

/// The raw bytes (and an optional note) for a (file, snapshot) selection. A free
/// function so it borrows `state`/`fs` immutably without tangling with `&mut self`.
fn slice_bytes(
    state: &State,
    fs: &OsFs,
    file_ix: usize,
    snapshot: Snapshot,
) -> (Vec<u8>, Option<SharedString>) {
    let Some(file) = state.files.get(file_ix) else {
        return (Vec::new(), None);
    };
    match snapshot {
        // The live pending slice — exactly what `logsnap diff` would print.
        Snapshot::Uncommitted => {
            let st = fs.stat(&file.path);
            let (from, ev) = resolve(file, &st);
            let note = note_for(ev);
            if ev.absent() {
                (Vec::new(), note)
            } else {
                let data = fs.read(&file.path).unwrap_or_default();
                (region(&data, from).bytes, note)
            }
        }
        // Re-read a checkpoint's committed byte range, like `logsnap diff --in`.
        Snapshot::Checkpoint(id) => {
            let entry = state
                .history
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.entries.iter().find(|e| e.path == file.path));
            match entry {
                None => (Vec::new(), Some("not part of this checkpoint".into())),
                Some(e) => {
                    let st = fs.stat(&e.path);
                    let available =
                        matches!(st, Some(s) if s.dev == e.dev && s.ino == e.ino && s.size >= e.to);
                    if !available {
                        (
                            Vec::new(),
                            Some("unavailable (file rotated/truncated since commit)".into()),
                        )
                    } else {
                        let data = fs.read(&e.path).unwrap_or_default();
                        (data[e.from as usize..e.to as usize].to_vec(), None)
                    }
                }
            }
        }
    }
}

/// A human note for an identity event, mirroring the CLI's stderr warnings.
fn note_for(ev: Event) -> Option<SharedString> {
    match ev {
        Event::Ok | Event::Appeared => None,
        Event::Missing => Some("not present".into()),
        Event::Disappeared => Some("disappeared since last seen".into()),
        Event::Truncated => Some("⚠ truncated (shrank) — reading from start".into()),
        Event::Rotated => {
            Some("⚠ identity changed (rotated/replaced) — reading the new file from start".into())
        }
    }
}

fn main() {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(720.)), cx)),
            // Wayland app_id / X11 WM_CLASS
            app_id: Some("logsnap-ui".into()),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                Theme::change(ThemeMode::Dark, Some(window), cx);
                // Always show scrollbars (the theme default auto-hides them on systems
                // that report a hide-scrollbars preference, which makes them invisible here).
                Theme::global_mut(cx).scrollbar_show = ScrollbarShow::Always;
                let view = cx.new(|_| LogViewer::new());
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open window");
        })
        .detach();
    });
}

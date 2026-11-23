//! Settings > Ghost Desktops: every sandbox the Engine runs, live.
//!
//! The Engine owns the pool (`sandbox.*`); this page only watches it. While
//! the page is on screen it reads `sandbox.list` once a second and each
//! sandbox's latest preview frame ten times a second (`afterSeq` skips
//! unchanged frames, and a sandbox renders preview frames only while
//! someone asks). Off screen it asks for nothing.
//!
//! A click, scroll or key on a tile goes to that sandbox as the person,
//! which takes it over for three seconds: the Agent's actions are refused
//! until the person stops, then it carries on. Nothing here touches the
//! person's own cursor or focus; the sandbox has no window to focus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use diri_client::DaemonClient;
use diri_proto::sandbox::{SandboxFrame, SandboxInfo, SandboxListResult};
use diri_proto::{EventName, Method};
use diri_ui::{Ink, Radius, SemanticColors, Typo};
use gpui::{
    AnyElement, Bounds, Context, FocusHandle, FontWeight, KeyDownEvent, MouseButton,
    MouseDownEvent, ObjectFit, Pixels, Render, RenderImage, ScrollWheelEvent, SharedString,
    StyledImage as _, Task, Window, canvas, div, img, prelude::*, px,
};
use serde_json::json;
use tokio::runtime::Runtime;

use crate::i18n::{t, tf};
use crate::store::{SessionStore, StoreRuntime};

/// How long after the last render the page keeps polling.
const WATCH_LINGER: Duration = Duration::from_millis(1500);
const FRAME_INTERVAL: Duration = Duration::from_millis(100);
const LIST_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

struct Tile {
    seq: u64,
    image: Arc<RenderImage>,
}

pub struct SandboxesPage {
    store: Arc<RwLock<SessionStore>>,
    store_runtime: Arc<StoreRuntime>,
    runtime: Arc<Runtime>,
    focus: FocusHandle,
    list: Option<SandboxListResult>,
    error: Option<String>,
    tiles: HashMap<String, Tile>,
    /// Frames replaced since the last paint; their textures are freed then.
    retired: Vec<Arc<RenderImage>>,
    /// Each tile's preview bounds from the last paint, to map clicks.
    bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// The tile the person last clicked: keys go there.
    controlled: Option<String>,
    watched_until: Instant,
    poll: Option<Task<()>>,
    /// Preview fixtures never poll.
    seeded: bool,
    /// Session titles for the screenshot fixture, which has no store.
    #[cfg(test)]
    preview_owners: HashMap<String, String>,
    _events: Task<()>,
}

impl SandboxesPage {
    pub fn new(
        store_runtime: Arc<StoreRuntime>,
        runtime: Arc<Runtime>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut events = {
            let _runtime = runtime.enter();
            store_runtime.client().events()
        };
        let events_task = cx.spawn(async move |this, cx| {
            loop {
                match events.recv().await {
                    Ok(event) if event.name == EventName::SANDBOX_CHANGED => {
                        if this.update(cx, |this, cx| this.refresh_list(cx)).is_err() {
                            return;
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Self {
            store: Arc::clone(&store_runtime.store),
            store_runtime,
            runtime,
            focus: cx.focus_handle(),
            list: None,
            error: None,
            tiles: HashMap::new(),
            retired: Vec::new(),
            bounds: Rc::default(),
            controlled: None,
            watched_until: Instant::now(),
            poll: None,
            seeded: false,
            #[cfg(test)]
            preview_owners: HashMap::new(),
            _events: events_task,
        }
    }

    /// Called when the tab is shown.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.watch(cx);
        self.refresh_list(cx);
    }

    fn client(&self) -> Arc<DaemonClient> {
        Arc::clone(self.store_runtime.client())
    }

    /// Keeps the page polling for a moment; cheap to call every frame.
    fn watch(&mut self, cx: &mut Context<Self>) {
        if self.seeded {
            return;
        }
        self.watched_until = Instant::now() + WATCH_LINGER;
        if self.poll.is_none() {
            self.poll = Some(self.start_polling(cx));
        }
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let client = self.client();
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let mut list_due = Instant::now();
            loop {
                let Ok(Some(wanted)) = this.update(cx, |this, _| {
                    if this.watched_until <= Instant::now() {
                        this.poll = None;
                        return None;
                    }
                    Some(
                        this.list
                            .as_ref()
                            .map(|list| {
                                list.sandboxes
                                    .iter()
                                    .map(|sandbox| {
                                        (
                                            sandbox.id.clone(),
                                            this.tiles.get(&sandbox.id).map(|tile| tile.seq),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default(),
                    )
                }) else {
                    return;
                };
                let list = if Instant::now() >= list_due {
                    list_due = Instant::now() + LIST_INTERVAL;
                    let client = Arc::clone(&client);
                    Some(
                        runtime
                            .spawn(async move {
                                client
                                    .request(Method::SANDBOX_LIST, Some(&json!({})), Some(REQUEST_TIMEOUT))
                                    .await
                            })
                            .await,
                    )
                } else {
                    None
                };
                let frames = {
                    let client = Arc::clone(&client);
                    runtime
                        .spawn(async move { fetch_frames(&client, wanted).await })
                        .await
                        .unwrap_or_default()
                };
                let alive = this
                    .update(cx, |this, cx| {
                        match list {
                            Some(Ok(Ok(value))) => match serde_json::from_value(value) {
                                Ok(list) => {
                                    this.set_list(list);
                                    this.error = None;
                                }
                                Err(error) => this.error = Some(error.to_string()),
                            },
                            Some(Ok(Err(error))) => this.error = Some(error.to_string()),
                            Some(Err(error)) => this.error = Some(error.to_string()),
                            None => {}
                        }
                        for (id, seq, image) in frames {
                            if let Some(old) = this.tiles.insert(id, Tile { seq, image }) {
                                this.retired.push(old.image);
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    return;
                }
                cx.background_executor().timer(FRAME_INTERVAL).await;
            }
        })
    }

    fn refresh_list(&mut self, cx: &mut Context<Self>) {
        if self.seeded {
            return;
        }
        let client = self.client();
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client.wait_until_connected(REQUEST_TIMEOUT).await?;
                    client
                        .request(Method::SANDBOX_LIST, Some(&json!({})), Some(REQUEST_TIMEOUT))
                        .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(value)) => match serde_json::from_value(value) {
                        Ok(list) => {
                            this.set_list(list);
                            this.error = None;
                        }
                        Err(error) => this.error = Some(error.to_string()),
                    },
                    Ok(Err(error)) => this.error = Some(error.to_string()),
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_list(&mut self, list: SandboxListResult) {
        let live: Vec<&str> = list.sandboxes.iter().map(|s| s.id.as_str()).collect();
        let gone: Vec<String> = self
            .tiles
            .keys()
            .filter(|id| !live.contains(&id.as_str()))
            .cloned()
            .collect();
        for id in gone {
            if let Some(tile) = self.tiles.remove(&id) {
                self.retired.push(tile.image);
            }
        }
        if self
            .controlled
            .as_ref()
            .is_some_and(|id| !live.contains(&id.as_str()))
        {
            self.controlled = None;
        }
        self.list = Some(list);
    }

    /// One action as the person; the Engine hands the sandbox to them for
    /// a moment.
    fn act(&mut self, id: &str, action: serde_json::Value, cx: &mut Context<Self>) {
        if self.seeded {
            return;
        }
        let mut params = action;
        params["sandboxID"] = json!(id);
        let client = self.client();
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client
                        .request(Method::SANDBOX_ACT, Some(&params), Some(Duration::from_secs(40)))
                        .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(Err(error)) = result {
                    this.error = Some(error.to_string());
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn stop(&mut self, id: String, cx: &mut Context<Self>) {
        let client = self.client();
        let runtime = Arc::clone(&self.runtime);
        cx.spawn(async move |this, cx| {
            let result = runtime
                .spawn(async move {
                    client
                        .request(
                            Method::SANDBOX_STOP,
                            Some(&json!({"sandboxID": id})),
                            Some(Duration::from_secs(20)),
                        )
                        .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(Err(error)) = result {
                    this.error = Some(error.to_string());
                }
                this.refresh_list(cx);
            });
        })
        .detach();
    }

    fn click(&mut self, info: &SandboxInfo, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(bounds) = self.bounds.borrow().get(&info.id).copied() else {
            return;
        };
        let Some((x, y)) = map_point(bounds, event.position, info.width, info.height) else {
            return;
        };
        self.controlled = Some(info.id.clone());
        self.act(&info.id, json!({"action": "click", "x": x, "y": y}), cx);
    }

    fn scroll(&mut self, info: &SandboxInfo, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(bounds) = self.bounds.borrow().get(&info.id).copied() else {
            return;
        };
        let Some((x, y)) = map_point(bounds, event.position, info.width, info.height) else {
            return;
        };
        let delta = event.delta.pixel_delta(px(16.0));
        // Wheel deltas point the other way from page scroll offsets.
        let (dx, dy) = (-f32::from(delta.x), -f32::from(delta.y));
        if dx.abs() < 0.5 && dy.abs() < 0.5 {
            return;
        }
        self.act(
            &info.id,
            json!({"action": "scroll", "x": x, "y": y, "dx": dx, "dy": dy}),
            cx,
        );
    }

    /// Keys go to the tile the person clicked, while it is still theirs.
    pub(crate) fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.controlled.clone() else {
            return false;
        };
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control {
            return false;
        }
        let action = match keystroke.key.as_str() {
            "escape" => {
                self.controlled = None;
                cx.notify();
                return true;
            }
            key => match dom_key(key) {
                Some(name) => json!({"action": "press", "key": name}),
                None => match keystroke.key_char.as_deref() {
                    Some(text) if !text.is_empty() => json!({"action": "type", "text": text}),
                    _ => return false,
                },
            },
        };
        self.act(&id, action, cx);
        cx.stop_propagation();
        true
    }

    fn colors(&self) -> SemanticColors {
        crate::app_theme::colors_in(&self.store.read().expect("session store lock poisoned"))
    }

    fn owner_name(&self, info: &SandboxInfo) -> String {
        let Some(owner) = &info.owner else {
            return t("settings.sandboxes.person").into();
        };
        #[cfg(test)]
        if let Some(title) = self.preview_owners.get(&owner.0) {
            return title.clone();
        }
        self.store
            .read()
            .expect("session store lock poisoned")
            .sessions()
            .get(owner)
            .map(|record| record.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| owner.0.clone())
    }

    /// Fixture for the settings screenshot: four sandboxes with real frames
    /// when `frames` holds JPEGs, grey tiles otherwise.
    #[cfg(test)]
    pub(crate) fn seed_preview(&mut self, frames: Vec<Vec<u8>>) {
        use diri_proto::DateMillis;
        use diri_proto::sandbox::{SandboxBackendKind, SandboxBackendStatus, SandboxUsage};
        self.seeded = true;
        let now = DateMillis::from(std::time::SystemTime::now());
        let pages = [
            ("Checkout flow", "http://localhost:3000/checkout"),
            ("Sign-up form", "http://localhost:5173/signup"),
            ("Dashboard", "http://localhost:4000/"),
            ("Docs search", "http://localhost:8080/docs?q=sandbox"),
        ];
        let sandboxes: Vec<SandboxInfo> = pages
            .iter()
            .enumerate()
            .map(|(index, (title, url))| SandboxInfo {
                id: format!("g{}", index + 1),
                // The person took over one an agent started; the others
                // belong to the agents testing them.
                owner: (index != 0).then(|| diri_proto::SessionId(format!("s{index}"))),
                label: Some((*title).into()),
                backend: SandboxBackendKind::Ghost,
                url: (*url).into(),
                title: (*title).into(),
                width: 1280,
                height: 800,
                started_at: now,
                last_active_at: now,
                spin_up_ms: [169, 169, 166, 184][index],
                usage: SandboxUsage {
                    memory_bytes: [68_157_440, 68_262_297, 68_262_297, 68_367_155][index],
                    processes: 4,
                    disk_bytes: 1_163_264,
                },
                human_until: (index == 1).then_some(now),
                sleeping: index == 3,
            })
            .collect();
        for (index, owner) in ["fix-signup-validation", "dashboard-charts", "docs-search"]
            .into_iter()
            .enumerate()
        {
            self.preview_owners
                .insert(format!("s{}", index + 1), owner.into());
        }
        for (sandbox, jpeg) in sandboxes.iter().zip(frames) {
            if let Some(image) = decode_jpeg(&jpeg) {
                self.tiles.insert(sandbox.id.clone(), Tile { seq: 1, image });
            }
        }
        self.list = Some(SandboxListResult {
            capacity: 4,
            sandboxes,
            backends: vec![SandboxBackendStatus {
                kind: SandboxBackendKind::Ghost,
                available: true,
                detail: "chrome-headless-shell".into(),
            }],
        });
    }

    fn tile(&self, info: &SandboxInfo, colors: SemanticColors, cx: &mut Context<Self>) -> AnyElement {
        let id = info.id.clone();
        let human = info.human_until.is_some();
        let controlled = self.controlled.as_deref() == Some(info.id.as_str());
        let bounds = Rc::clone(&self.bounds);
        let probe_id = id.clone();
        let probe = canvas(
            move |frame, _, _| {
                bounds.borrow_mut().insert(probe_id.clone(), frame);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        let click_info = info.clone();
        let scroll_info = info.clone();
        let mut preview = div()
            .id(SharedString::from(format!("sandbox-preview-{id}")))
            .relative()
            .w_full()
            .aspect_ratio(info.width.max(1) as f32 / info.height.max(1) as f32)
            .rounded(px(8.0))
            .overflow_hidden()
            .bg(colors.primary.alpha(0.06))
            .border_1()
            .border_color(if controlled || human {
                Ink::ATTENTION.alpha(0.9)
            } else {
                colors.primary.alpha(0.08)
            })
            .cursor_pointer()
            .child(probe)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.click(&click_info, event, cx);
                }),
            )
            .on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                this.scroll(&scroll_info, event, cx);
                cx.stop_propagation();
            }));
        preview = match self.tiles.get(&info.id) {
            Some(tile) => preview.child(
                img(tile.image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            ),
            None => preview.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.0))
                    .text_color(colors.tertiary)
                    .child(t("settings.sandboxes.running")),
            ),
        };
        if human || info.sleeping {
            preview = preview.child(
                div()
                    .absolute()
                    .top(px(8.0))
                    .left(px(8.0))
                    .px(px(7.0))
                    .py(px(2.0))
                    .rounded(px(Radius::ROW))
                    .bg(if human {
                        Ink::ATTENTION
                    } else {
                        gpui::rgba(0x00000099)
                    })
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(gpui::white())
                    .child(if human {
                        t("settings.sandboxes.you")
                    } else {
                        t("settings.sandboxes.asleep")
                    }),
            );
        }
        let usage = info.usage;
        let readout = [
            tf("settings.sandboxes.memory", &[("memory", &bytes(usage.memory_bytes))]),
            tf("settings.sandboxes.disk", &[("disk", &bytes(usage.disk_bytes))]),
            tf("settings.sandboxes.processes", &[("count", &usage.processes)]),
            tf("settings.sandboxes.ready_in", &[("ms", &info.spin_up_ms)]),
        ];
        let last = readout.len().saturating_sub(1);
        let name = info
            .label
            .clone()
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| self.owner_name(info));
        let stop_id = id.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .min_w(px(0.0))
            .child(preview)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(colors.primary)
                                    .truncate()
                                    .child(format!("{name} · {}", self.owner_name(info))),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(colors.secondary)
                                    .truncate()
                                    .child(info.url.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(Typo::META.size))
                                    .text_color(colors.tertiary)
                                    // Wrap between facts, never inside
                                    // one ("169 / ms").
                                    .flex()
                                    .flex_wrap()
                                    .children(readout.into_iter().enumerate().map(
                                        |(index, fact)| {
                                            let fact = if index < last {
                                                format!("{fact} ·\u{a0}")
                                            } else {
                                                fact
                                            };
                                            div().whitespace_nowrap().child(fact)
                                        },
                                    )),
                            ),
                    )
                    .child(
                        button(
                            SharedString::from(format!("sandbox-stop-{id}")),
                            t("settings.sandboxes.stop"),
                            colors,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.stop(stop_id.clone(), cx);
                        })),
                    ),
            )
            .into_any_element()
    }
}

/// Frames newer than what each tile shows, decoded off the UI thread.
async fn fetch_frames(
    client: &DaemonClient,
    wanted: Vec<(String, Option<u64>)>,
) -> Vec<(String, u64, Arc<RenderImage>)> {
    let mut out = Vec::new();
    for (id, after) in wanted {
        let mut params = json!({"sandboxID": id});
        if let Some(after) = after {
            params["afterSeq"] = json!(after);
        }
        let Ok(value) = client
            .request(Method::SANDBOX_FRAME, Some(&params), Some(REQUEST_TIMEOUT))
            .await
        else {
            continue;
        };
        let Ok(frame) = serde_json::from_value::<SandboxFrame>(value) else {
            continue;
        };
        let Some(jpeg) = frame
            .jpeg
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        else {
            continue;
        };
        if let Some(image) = decode_jpeg(&jpeg) {
            out.push((id, frame.seq, image));
        }
    }
    out
}

fn decode_jpeg(jpeg: &[u8]) -> Option<Arc<RenderImage>> {
    let mut rgba = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()?
        .into_rgba8();
    // GPUI paints BGRA.
    let raw: &mut [u8] = &mut rgba;
    for pixel in raw.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(smallvec::smallvec![
        image::Frame::new(rgba)
    ])))
}

/// A window point inside a tile's preview, in the sandbox's viewport pixels.
fn map_point(
    bounds: Bounds<Pixels>,
    point: gpui::Point<Pixels>,
    width: u32,
    height: u32,
) -> Option<(f64, f64)> {
    let w = f64::from(f32::from(bounds.size.width));
    let h = f64::from(f32::from(bounds.size.height));
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let x = (f64::from(f32::from(point.x - bounds.origin.x)) / w).clamp(0.0, 1.0);
    let y = (f64::from(f32::from(point.y - bounds.origin.y)) / h).clamp(0.0, 1.0);
    Some(((x * f64::from(width)).round(), (y * f64::from(height)).round()))
}

/// GPUI key names to DOM key names, for the keys that type no text.
fn dom_key(key: &str) -> Option<&'static str> {
    Some(match key {
        "enter" => "Enter",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        _ => return None,
    })
}

fn bytes(value: u64) -> String {
    let mb = value as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else if mb >= 10.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    colors: SemanticColors,
) -> gpui::Stateful<gpui::Div> {
    let label = label.into();
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.clone())
        .flex_none()
        .px(px(10.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .border_1()
        .border_color(colors.primary.alpha(0.1))
        .text_size(px(12.0))
        .text_color(colors.primary)
        .cursor_pointer()
        .hover(move |style| style.bg(colors.primary.alpha(0.06)))
        .active(move |style| style.bg(colors.primary.alpha(0.1)))
        .child(label)
}

impl Render for SandboxesPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.retired.drain(..) {
            cx.drop_image(image, Some(window));
        }
        self.watch(cx);
        let colors = self.colors();
        let list = self.list.clone();
        let used = list.as_ref().map_or(0, |list| list.sandboxes.len());
        let capacity = list.as_ref().map_or(diri_proto::sandbox::DEFAULT_CAPACITY, |l| l.capacity);
        let mut page = div()
            .id("sandboxes-page")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event, _, cx| {
                this.handle_key(event, cx);
            }))
            .px(px(24.0))
            .pt(px(18.0))
            .pb(px(24.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .text_color(colors.primary)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t("settings.tab.sandboxes")),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.secondary)
                            .child(tf(
                                "settings.sandboxes.capacity",
                                &[("used", &used), ("total", &capacity)],
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(colors.secondary)
                    .child(t("settings.sandboxes.intro")),
            );
        if let Some(error) = &self.error {
            page = page.child(
                div()
                    .text_size(px(12.0))
                    .text_color(Ink::DANGER)
                    .child(error.clone()),
            );
        }
        let sandboxes = list
            .as_ref()
            .map(|list| list.sandboxes.clone())
            .unwrap_or_default();
        if sandboxes.is_empty() {
            page = page.child(
                div()
                    .rounded(px(Radius::ROW))
                    .border_1()
                    .border_color(colors.primary.alpha(0.065))
                    .bg(colors.primary.alpha(0.02))
                    .px(px(12.0))
                    .py(px(16.0))
                    .text_size(px(12.0))
                    .text_color(colors.tertiary)
                    .child(t("settings.sandboxes.empty")),
            );
        } else {
            let mut grid = div().grid().grid_cols(2).gap(px(16.0));
            for info in &sandboxes {
                grid = grid.child(self.tile(info, colors, cx));
            }
            page = page.child(grid);
            let memory: u64 = sandboxes.iter().map(|s| s.usage.memory_bytes).sum();
            let disk: u64 = sandboxes.iter().map(|s| s.usage.disk_bytes).sum();
            page = page.child(
                div()
                    .text_size(px(12.0))
                    .text_color(colors.secondary)
                    .child(tf(
                        "settings.sandboxes.total",
                        &[("memory", &bytes(memory)), ("disk", &bytes(disk))],
                    )),
            );
        }
        for backend in list.iter().flat_map(|list| list.backends.iter()) {
            if !backend.available && backend.kind == diri_proto::sandbox::SandboxBackendKind::Ghost {
                page = page.child(
                    div()
                        .text_size(px(12.0))
                        .text_color(colors.tertiary)
                        .child(tf(
                            "settings.sandboxes.unavailable",
                            &[("detail", &backend.detail)],
                        )),
                );
            }
        }
        page
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_map_into_the_viewport() {
        let bounds = Bounds::new(gpui::point(px(100.0), px(50.0)), gpui::size(px(320.0), px(200.0)));
        assert_eq!(
            map_point(bounds, gpui::point(px(260.0), px(150.0)), 1280, 800),
            Some((640.0, 400.0))
        );
        assert_eq!(
            map_point(bounds, gpui::point(px(0.0), px(0.0)), 1280, 800),
            Some((0.0, 0.0))
        );
    }

    #[test]
    fn readouts_are_short() {
        assert_eq!(bytes(1_163_264), "1.1 MB");
        assert_eq!(bytes(68_157_440), "65 MB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn keys_without_text_map_to_dom_names() {
        assert_eq!(dom_key("enter"), Some("Enter"));
        assert_eq!(dom_key("a"), None);
    }
}

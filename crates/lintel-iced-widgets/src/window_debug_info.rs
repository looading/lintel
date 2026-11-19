use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::mouse_area::mouse_area;
use lintel_lib_iced::iced::alignment::{Horizontal, Vertical};
use lintel_lib_iced::iced::time;
use lintel_lib_iced::iced::widget::{button, column, container, row, stack, text};
use lintel_lib_iced::iced::{self, Alignment, Color, Element, Length, Point, Size, Subscription, Task, Theme, Vector};
use lintel_lib_iced::mouse;
use lintel_lib_iced::widget::space::{horizontal, vertical};

const DEFAULT_SYNC_INTERVAL_MS: u64 = 300;
const DEFAULT_PANEL_WIDTH: f32 = 320.0;
const DEFAULT_MARGIN: f32 = 12.0;
const PANEL_PADDING: f32 = 12.0;
const PANEL_BORDER_RADIUS: f32 = 10.0;

/// 窗口调试信息组件。
///
/// 该组件会以 `stack` 的方式叠加到内容右上角，并显示当前窗口的基础状态。
/// 调用方需要：
///
/// - 在窗口创建完成后调用 `set_window_id(Some(window_id))` 显式绑定目标窗口
/// - 在 `update` 中转发 [`WindowDebugInfoMessage`]
/// - 在 `subscription` 中合并 `subscription()`
/// - 在 `view` 中调用 `overlay()`
#[derive(Debug, Clone)]
pub struct WindowDebugInfo {
  window_id: Option<iced::window::Id>,
  snapshot: WindowDebugSnapshot,
  drag_state: DragState,
  frame_stats: FrameStats,
  track_ui_fps: bool,
  offset: Vector,
  collapsed: bool,
  exera_info: HashMap<String, String>,
  panel_width: f32,
  sync_interval: Duration,
}

impl Default for WindowDebugInfo {
  fn default() -> Self {
    Self::new(None)
  }
}

impl WindowDebugInfo {
  pub fn new(window_id: Option<iced::window::Id>) -> Self {
    Self {
      window_id,
      snapshot: WindowDebugSnapshot {
        window_id,
        ..Default::default()
      },
      drag_state: DragState::default(),
      frame_stats: FrameStats::default(),
      track_ui_fps: false,
      offset: Vector::new(0.0, 0.0),
      collapsed: false,
      exera_info: HashMap::new(),
      panel_width: DEFAULT_PANEL_WIDTH,
      sync_interval: Duration::from_millis(DEFAULT_SYNC_INTERVAL_MS),
    }
  }

  pub fn with_collapsed(mut self, collapsed: bool) -> Self {
    self.collapsed = collapsed;
    self
  }

  pub fn with_panel_width(mut self, panel_width: f32) -> Self {
    self.panel_width = panel_width.max(220.0);
    self
  }

  pub fn with_exera_info(mut self, exera_info: HashMap<String, String>) -> Self {
    self.exera_info = exera_info;
    self
  }

  pub fn with_sync_interval(mut self, interval: Duration) -> Self {
    self.sync_interval = interval;
    self
  }

  pub fn window_id(&self) -> Option<iced::window::Id> {
    self.window_id
  }

  pub fn snapshot(&self) -> &WindowDebugSnapshot {
    &self.snapshot
  }

  pub fn is_collapsed(&self) -> bool {
    self.collapsed
  }

  pub fn exera_info(&self) -> &HashMap<String, String> {
    &self.exera_info
  }

  pub fn set_window_id(&mut self, window_id: Option<iced::window::Id>) -> Task<WindowDebugInfoMessage> {
    if self.window_id == window_id {
      return Task::none();
    }

    self.window_id = window_id;
    self.snapshot.window_id = window_id;
    self.frame_stats = FrameStats::default();
    self.snapshot.ui_fps = 0.0;
    self.sync()
  }

  pub fn set_exera_info(&mut self, exera_info: HashMap<String, String>) {
    self.exera_info = exera_info;
  }

  pub fn sync(&self) -> Task<WindowDebugInfoMessage> {
    let Some(window_id) = self.window_id else {
      return Task::none();
    };

    let focused = self.snapshot.window_focused;
    let ui_fps = self.snapshot.ui_fps;
    iced::window::position(window_id).then(move |position| {
      iced::window::size(window_id).then(move |size| {
        iced::window::mode(window_id).then(move |mode| {
          iced::window::is_maximized(window_id).then(move |is_maximized| {
            iced::window::is_minimized(window_id).map(move |is_minimized| {
              WindowDebugInfoMessage::SnapshotSynced(WindowDebugSnapshot {
                window_id: Some(window_id),
                window_focused: focused,
                window_position: position,
                window_size: Some(size),
                is_maximized,
                is_minimized: is_minimized.unwrap_or(false),
                is_fullscreen: mode == iced::window::Mode::Fullscreen,
                mode: Some(mode),
                ui_fps,
              })
            })
          })
        })
      })
    })
  }

  pub fn update(&mut self, message: WindowDebugInfoMessage) -> Task<WindowDebugInfoMessage> {
    match message {
      WindowDebugInfoMessage::SyncRequested => self.sync(),
      WindowDebugInfoMessage::SnapshotSynced(snapshot) => {
        self.snapshot = snapshot;
        Task::none()
      }
      WindowDebugInfoMessage::FocusChanged(window_id, focused) => {
        if self.window_id == Some(window_id) {
          self.snapshot.window_focused = focused;
          return self.sync();
        }
        Task::none()
      }
      WindowDebugInfoMessage::RedrawRequested(window_id, now) => {
        if !self.track_ui_fps || self.window_id != Some(window_id) {
          return Task::none();
        }
        self.snapshot.ui_fps = self.frame_stats.record(now);
        Task::none()
      }
      WindowDebugInfoMessage::CursorMoved(window_id, position) => {
        if self.window_id != Some(window_id) {
          return Task::none();
        }
        if self.drag_state.dragging {
          if let Some(last_cursor_position) = self.drag_state.last_cursor_position {
            let delta = position - last_cursor_position;
            self.offset.x = (self.offset.x - delta.x).max(0.0);
            self.offset.y = (self.offset.y + delta.y).max(0.0);
          }
        }
        self.drag_state.last_cursor_position = Some(position);
        Task::none()
      }
      WindowDebugInfoMessage::DragStarted => {
        self.drag_state.dragging = true;
        Task::none()
      }
      WindowDebugInfoMessage::DragEnded(window_id) => {
        if self.window_id != Some(window_id) {
          return Task::none();
        }
        self.drag_state.dragging = false;
        Task::none()
      }
      WindowDebugInfoMessage::ResetOffset => {
        self.offset = Vector::new(0.0, 0.0);
        Task::none()
      }
      WindowDebugInfoMessage::ToggleCollapsed => {
        self.collapsed = !self.collapsed;
        Task::none()
      }
      WindowDebugInfoMessage::ToggleUiFps => {
        self.track_ui_fps = !self.track_ui_fps;
        self.frame_stats = FrameStats::default();
        self.snapshot.ui_fps = 0.0;
        Task::none()
      }
      WindowDebugInfoMessage::Ignore => Task::none(),
    }
  }

  pub fn subscription(&self) -> Subscription<WindowDebugInfoMessage> {
    let event_subscription = if let Some(window_id) = self.window_id {
      let mut subscriptions = vec![
        iced::event::listen_raw(|event, _status, event_window_id| match event {
          iced::Event::Window(iced::window::Event::Focused) => {
            Some(WindowDebugInfoMessage::FocusChanged(event_window_id, true))
          }
          iced::Event::Window(iced::window::Event::Unfocused) => {
            Some(WindowDebugInfoMessage::FocusChanged(event_window_id, false))
          }
          _ => None,
        })
        .with(window_id)
        .filter_map(|(window_id, message)| match message {
          WindowDebugInfoMessage::FocusChanged(event_window_id, focused) if window_id == event_window_id => {
            Some(WindowDebugInfoMessage::FocusChanged(event_window_id, focused))
          }
          _ => None,
        }),
      ];

      if self.drag_state.dragging {
        subscriptions.push(
          iced::event::listen_raw(|event, _status, event_window_id| match event {
            iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
              Some(WindowDebugInfoMessage::CursorMoved(event_window_id, position))
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
              Some(WindowDebugInfoMessage::DragEnded(event_window_id))
            }
            _ => None,
          })
          .with(window_id)
          .filter_map(|(window_id, message)| match message {
            WindowDebugInfoMessage::CursorMoved(event_window_id, position) if window_id == event_window_id => {
              Some(WindowDebugInfoMessage::CursorMoved(event_window_id, position))
            }
            WindowDebugInfoMessage::DragEnded(event_window_id) if window_id == event_window_id => {
              Some(WindowDebugInfoMessage::DragEnded(event_window_id))
            }
            _ => None,
          }),
        );
      }

      if self.track_ui_fps {
        subscriptions.push(
          iced::event::listen_raw(|event, _status, event_window_id| match event {
            iced::Event::Window(iced::window::Event::RedrawRequested(_)) => {
              Some(WindowDebugInfoMessage::RedrawRequested(event_window_id, Instant::now()))
            }
            _ => None,
          })
          .with(window_id)
          .filter_map(|(window_id, message)| match message {
            WindowDebugInfoMessage::RedrawRequested(event_window_id, now) if window_id == event_window_id => {
              Some(WindowDebugInfoMessage::RedrawRequested(event_window_id, now))
            }
            _ => None,
          }),
        );
      }

      Subscription::batch(subscriptions)
    } else {
      Subscription::none()
    };

    Subscription::batch([
      time::every(self.sync_interval).map(|_| WindowDebugInfoMessage::SyncRequested),
      event_subscription,
    ])
  }

  pub fn overlay<'a, Message>(
    &'a self,
    content: impl Into<Element<'a, Message>>,
    map: impl Fn(WindowDebugInfoMessage) -> Message + Clone + 'a,
  ) -> Element<'a, Message>
  where
    Message: Clone + 'a,
  {
    self.overlay_with_exera_info(content, map, self.exera_info.clone())
  }

  pub fn overlay_with_exera_info<'a, Message>(
    &'a self,
    content: impl Into<Element<'a, Message>>,
    map: impl Fn(WindowDebugInfoMessage) -> Message + Clone + 'a,
    exera_info: HashMap<String, String>,
  ) -> Element<'a, Message>
  where
    Message: Clone + 'a,
  {
    let content = content.into();
    let panel = self.panel(map.clone(), exera_info);
    let overlay = column![
      vertical().height(Length::Fixed(DEFAULT_MARGIN + self.offset.y)),
      row![
        horizontal().width(Length::Fill),
        panel,
        horizontal().width(Length::Fixed(DEFAULT_MARGIN + self.offset.x)),
      ]
      .width(Length::Fill)
      .align_y(Alignment::Start),
      vertical().height(Length::Fill),
    ]
    .width(Length::Fill)
    .height(Length::Fill);

    stack![content, overlay].into()
  }

  fn panel<'a, Message>(
    &'a self,
    map: impl Fn(WindowDebugInfoMessage) -> Message + Clone + 'a,
    exera_info: HashMap<String, String>,
  ) -> Element<'a, Message>
  where
    Message: Clone + 'a,
  {
    let drag_interaction = if self.drag_state.dragging {
      mouse::Interaction::Grabbing
    } else {
      mouse::Interaction::Grab
    };

    let title_bar = mouse_area(
      container(
        column![
          row![
            text("WindowInfo").size(14).color(Color::WHITE),
            horizontal().width(Length::Fill),
          ]
          .align_y(Alignment::Center),
          row![
            button(text(if self.track_ui_fps { "FPS Off" } else { "FPS On" }).size(12))
              .padding([2, 8])
              .style(button::secondary)
              .on_press(map.clone()(WindowDebugInfoMessage::ToggleUiFps)),
            button(text(if self.collapsed { "Expand" } else { "Collapse" }).size(12))
              .padding([2, 8])
              .style(button::secondary)
              .on_press(map.clone()(WindowDebugInfoMessage::ToggleCollapsed)),
          ]
          .align_y(Alignment::Center)
          .spacing(8),
          row![
            button(text("Reset").size(12))
              .padding([2, 8])
              .style(button::secondary)
              .on_press(map.clone()(WindowDebugInfoMessage::ResetOffset)),
            button(text("Refresh").size(12))
              .padding([2, 8])
              .style(button::secondary)
              .on_press(map.clone()(WindowDebugInfoMessage::SyncRequested)),
          ]
          .align_y(Alignment::Center)
          .spacing(8),
        ]
        .spacing(8),
      )
      .padding([8, 10])
      .style(title_bar_style),
      Some("window_debug_info_drag".to_string()),
    )
    .on_press(map.clone()(WindowDebugInfoMessage::DragStarted))
    .interaction(drag_interaction);

    let mut panel_content = column![title_bar].spacing(0);
    if !self.collapsed {
      let mut detail_rows = vec![
        self.info_row("window_id", format_window_id(self.snapshot.window_id)),
        self.info_row("focused", self.snapshot.window_focused.to_string()),
        self.info_row("size", format_size(self.snapshot.window_size)),
        self.info_row("position", format_point(self.snapshot.window_position)),
        self.info_row("maximized", self.snapshot.is_maximized.to_string()),
        self.info_row("minimized", self.snapshot.is_minimized.to_string()),
        self.info_row("fullscreen", self.snapshot.is_fullscreen.to_string()),
        self.info_row("mode", format_mode(self.snapshot.mode)),
        self.info_row("offset", format!("{:.1}, {:.1}", self.offset.x, self.offset.y)),
      ];
      if self.track_ui_fps {
        detail_rows.insert(8, self.info_row("ui_fps", format_fps(self.snapshot.ui_fps)));
      }

      let mut exera_info_entries = exera_info.into_iter().collect::<Vec<_>>();
      exera_info_entries.sort_by(|(left_key, _), (right_key, _)| left_key.cmp(right_key));

      if !exera_info_entries.is_empty() {
        detail_rows.push(self.separator_row());
        detail_rows.push(self.info_row("exeraInfo", format!("{}", exera_info_entries.len())));
        for (key, value) in exera_info_entries {
          detail_rows.push(self.info_row(key, value));
        }
      }

      panel_content = panel_content.push(container(column(detail_rows).spacing(6)).padding(PANEL_PADDING));
    }

    container(panel_content)
      .width(Length::Fixed(self.panel_width))
      .style(panel_style)
      .into()
  }

  fn info_row<'a, Message>(&self, label: impl Into<String>, value: String) -> Element<'a, Message>
  where
    Message: Clone + 'a,
  {
    row![
      text(label.into())
        .width(Length::Fixed(88.0))
        .size(12)
        .color(Color::from_rgb8(0x9C, 0xA3, 0xAF)),
      text(value)
        .width(Length::Fill)
        .size(12)
        .align_x(Horizontal::Right)
        .align_y(Vertical::Center)
        .color(Color::from_rgb8(0xF3, 0xF4, 0xF6)),
    ]
    .align_y(Alignment::Center)
    .into()
  }

  fn separator_row<'a, Message>(&self) -> Element<'a, Message>
  where
    Message: Clone + 'a,
  {
    container("")
      .height(Length::Fixed(1.0))
      .width(Length::Fill)
      .style(|_: &Theme| container::Style {
        background: Some(iced::Background::Color(Color::from_rgba8(0xFF, 0xFF, 0xFF, 0.08))),
        ..Default::default()
      })
      .into()
  }
}

#[derive(Debug, Clone, Default)]
struct DragState {
  dragging: bool,
  last_cursor_position: Option<Point>,
}

#[derive(Debug, Clone, Default)]
struct FrameStats {
  redraw_timestamps: VecDeque<Instant>,
}

impl FrameStats {
  fn record(&mut self, now: Instant) -> f32 {
    self.redraw_timestamps.push_back(now);
    let window = Duration::from_secs(1);
    while let Some(front) = self.redraw_timestamps.front().copied() {
      if now.duration_since(front) <= window {
        break;
      }
      self.redraw_timestamps.pop_front();
    }
    self.redraw_timestamps.len() as f32
  }
}

#[derive(Debug, Clone)]
pub enum WindowDebugInfoMessage {
  SyncRequested,
  SnapshotSynced(WindowDebugSnapshot),
  FocusChanged(iced::window::Id, bool),
  RedrawRequested(iced::window::Id, Instant),
  CursorMoved(iced::window::Id, Point),
  DragStarted,
  DragEnded(iced::window::Id),
  ResetOffset,
  ToggleCollapsed,
  ToggleUiFps,
  Ignore,
}

#[derive(Debug, Clone, Default)]
pub struct WindowDebugSnapshot {
  pub window_id: Option<iced::window::Id>,
  pub window_focused: bool,
  pub window_size: Option<Size>,
  pub window_position: Option<Point>,
  pub is_maximized: bool,
  pub is_minimized: bool,
  pub is_fullscreen: bool,
  pub mode: Option<iced::window::Mode>,
  pub ui_fps: f32,
}

fn format_window_id(window_id: Option<iced::window::Id>) -> String {
  window_id
    .map(|id| format!("{id:?}"))
    .unwrap_or_else(|| "None".to_string())
}

fn format_size(size: Option<Size>) -> String {
  size
    .map(|size| format!("{:.1} x {:.1}", size.width, size.height))
    .unwrap_or_else(|| "None".to_string())
}

fn format_point(position: Option<Point>) -> String {
  position
    .map(|point| format!("{:.1}, {:.1}", point.x, point.y))
    .unwrap_or_else(|| "None".to_string())
}

fn format_mode(mode: Option<iced::window::Mode>) -> String {
  mode
    .map(|mode| format!("{mode:?}"))
    .unwrap_or_else(|| "None".to_string())
}

fn format_fps(fps: f32) -> String {
  format!("{fps:.0}")
}

fn panel_style(_theme: &Theme) -> container::Style {
  container::Style {
    background: Some(iced::Background::Color(Color::from_rgba8(0x11, 0x18, 0x27, 0.92))),
    border: iced::Border {
      radius: PANEL_BORDER_RADIUS.into(),
      width: 1.0,
      color: Color::from_rgba8(0xFF, 0xFF, 0xFF, 0.12),
    },
    text_color: Some(Color::WHITE),
    ..Default::default()
  }
}

fn title_bar_style(_theme: &Theme) -> container::Style {
  container::Style {
    background: Some(iced::Background::Color(Color::from_rgba8(0x1F, 0x29, 0x37, 0.96))),
    border: iced::Border {
      radius: PANEL_BORDER_RADIUS.into(),
      width: 0.0,
      color: Color::TRANSPARENT,
    },
    text_color: Some(Color::WHITE),
    ..Default::default()
  }
}

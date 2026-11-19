/// figma: https://www.figma.com/design/R5n6jHH5s2VJmiYzhNNJR8/%F0%9F%96%A5%EF%B8%8F%E3%80%90%E6%9E%81%E5%85%89%E7%BB%84%E4%BB%B6%E5%BA%93%E3%80%91AURORA-Design-UI-Kit?node-id=10473-102685&t=XmRceip4eilxSakl-0
/// 全局 toast 组件
/// 参考 example toast 使用
use std::collections::BTreeMap;
use std::fmt;

use lintel_lib_iced::widget::text::LineHeight;
use lintel_lib_iced::{Color, Font, Padding, Shadow, border, color, iced};

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, Operation, Tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::mouse;
use iced::theme;
use iced::time::{self, Duration, Instant};
use iced::widget::{container, row, text};
use iced::window;
use iced::{Alignment, Element, Event, Fill, Length, Point, Rectangle, Renderer, Size, Theme, Vector};
use lintel_lib_iced::widget::svg::{self, Handle};

use crate::svg_resources;

pub const DEFAULT_TIMEOUT: u64 = 5;
pub const DEFAULT_MAX_TOASTS: usize = 10;
pub type ToastId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
  #[default]
  Info,
  Warn,
  Success,
  Danger,
}

impl Status {
  pub const ALL: &'static [Self] = &[Self::Info, Self::Warn, Self::Success, Self::Danger];
}

impl fmt::Display for Status {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Status::Info => "Info",
      Status::Warn => "Warn",
      Status::Success => "Success",
      Status::Danger => "Danger",
    }
    .fmt(f)
  }
}

#[derive(Debug)]
pub struct StatusIcon {
  success_handle: Handle,
  info_handle: Handle,
  warn_handle: Handle,
  danger_handle: Handle,
}

impl StatusIcon {
  pub fn new() -> Self {
    Self {
      success_handle: svg_resources::toast_success(),
      info_handle: svg_resources::toast_info(),
      warn_handle: svg_resources::toast_warn(),
      danger_handle: svg_resources::toast_danger(),
    }
  }
}

impl StatusIcon {
  pub fn get(&self, status: &Status) -> Handle {
    match status {
      Status::Success => self.success_handle.clone(),
      Status::Info => self.info_handle.clone(),
      Status::Warn => self.warn_handle.clone(),
      Status::Danger => self.danger_handle.clone(),
    }
  }
}

#[derive(Debug, Clone, Default)]
pub struct Toast {
  pub body: String,
  pub status: Status,
}

pub struct Manager<'a, Message> {
  content: Element<'a, Message>,
  toast_ids: Vec<ToastId>,
  toasts: Vec<Element<'a, Message>>,
  timeout_secs: u64,
  on_close: Box<dyn Fn(ToastId) -> Message + 'a>,
}

impl<'a, Message> Manager<'a, Message>
where
  Message: 'a,
{
  pub fn new(
    content: impl Into<Element<'a, Message>>,
    toasts: impl IntoIterator<Item = (ToastId, &'a Toast)>,
    status_icon: &'a StatusIcon,
    on_close: impl Fn(ToastId) -> Message + 'a,
  ) -> Self {
    let mut toast_elements = vec![];
    let mut toast_ids = vec![];
    for (id, toast) in toasts {
      toast_ids.push(id);
      toast_elements.push(
        container(
          row![
            row![svg::Svg::new(status_icon.get(&toast.status)).width(16).height(20)].align_y(Alignment::Start),
            text(toast.body.as_str())
              .line_height(LineHeight::Absolute(20.into()))
              .size(14),
          ]
          .align_y(Alignment::Start)
          .spacing(8.0),
        )
        .width(Length::Shrink)
        .padding(Padding::from([12.0, 16.0]))
        .style(match toast.status {
          Status::Success => success,
          Status::Warn => warn,
          Status::Info => info,
          Status::Danger => danger,
        })
        .into(),
      );
    }

    Self {
      content: content.into(),
      toast_ids,
      toasts: toast_elements,
      timeout_secs: DEFAULT_TIMEOUT,
      on_close: Box::new(on_close),
    }
  }

  pub fn timeout(self, seconds: u64) -> Self {
    Self {
      timeout_secs: seconds,
      ..self
    }
  }
}

impl<Message> Widget<Message, Theme, Renderer> for Manager<'_, Message> {
  fn size(&self) -> Size<Length> {
    self.content.as_widget().size()
  }

  fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
    self
      .content
      .as_widget_mut()
      .layout(&mut tree.children[0], renderer, limits)
  }

  fn tag(&self) -> widget::tree::Tag {
    struct Marker;
    widget::tree::Tag::of::<Marker>()
  }

  fn state(&self) -> widget::tree::State {
    // 使用 ToastId -> Instant，避免 Vec remove 导致的索引错配（错删/计时器错绑）。
    widget::tree::State::new(BTreeMap::<ToastId, Instant>::new())
  }

  fn children(&self) -> Vec<Tree> {
    std::iter::once(Tree::new(&self.content))
      .chain(self.toasts.iter().map(Tree::new))
      .collect()
  }

  fn diff(&self, tree: &mut Tree) {
    let instants = tree.state.downcast_mut::<BTreeMap<ToastId, Instant>>();

    // 移除已经不存在的 toast 对应的计时状态
    instants.retain(|id, _| self.toast_ids.iter().any(|existing| existing == id));

    // 新增的 toast 初始化计时
    for id in &self.toast_ids {
      instants.entry(*id).or_insert_with(Instant::now);
    }

    tree.diff_children(
      &std::iter::once(&self.content)
        .chain(self.toasts.iter())
        .collect::<Vec<_>>(),
    );
  }

  fn operate(&mut self, state: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
    operation.container(None, layout.bounds());
    operation.traverse(&mut |operation| {
      self
        .content
        .as_widget_mut()
        .operate(&mut state.children[0], layout, renderer, operation);
    });
  }

  fn update(
    &mut self,
    state: &mut Tree,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    renderer: &Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
    viewport: &Rectangle,
  ) {
    self.content.as_widget_mut().update(
      &mut state.children[0],
      event,
      layout,
      cursor,
      renderer,
      clipboard,
      shell,
      viewport,
    );
  }

  fn draw(
    &self,
    state: &Tree,
    renderer: &mut Renderer,
    theme: &Theme,
    style: &renderer::Style,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
  ) {
    self
      .content
      .as_widget()
      .draw(&state.children[0], renderer, theme, style, layout, cursor, viewport);
  }

  fn mouse_interaction(
    &self,
    state: &Tree,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
    renderer: &Renderer,
  ) -> mouse::Interaction {
    self
      .content
      .as_widget()
      .mouse_interaction(&state.children[0], layout, cursor, viewport, renderer)
  }

  fn overlay<'b>(
    &'b mut self,
    state: &'b mut Tree,
    layout: Layout<'b>,
    renderer: &Renderer,
    viewport: &Rectangle,
    translation: Vector,
  ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
    let instants = state.state.downcast_mut::<BTreeMap<ToastId, Instant>>();

    let (content_state, toasts_state) = state.children.split_at_mut(1);

    let content = self
      .content
      .as_widget_mut()
      .overlay(&mut content_state[0], layout, renderer, viewport, translation);

    let toasts = (!self.toasts.is_empty()).then(|| {
      overlay::Element::new(Box::new(Overlay {
        position: layout.bounds().position() + translation,
        viewport: *viewport,
        toast_ids: &self.toast_ids,
        toasts: &mut self.toasts,
        state: toasts_state,
        instants,
        on_close: &self.on_close,
        timeout_secs: self.timeout_secs,
      }))
    });
    let overlays = content.into_iter().chain(toasts).collect::<Vec<_>>();

    (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
  }
}

struct Overlay<'a, 'b, Message> {
  position: Point,
  viewport: Rectangle,
  toast_ids: &'b [ToastId],
  toasts: &'b mut [Element<'a, Message>],
  state: &'b mut [Tree],
  instants: &'b mut BTreeMap<ToastId, Instant>,
  on_close: &'b dyn Fn(ToastId) -> Message,
  timeout_secs: u64,
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for Overlay<'_, '_, Message> {
  fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
    let limits = layout::Limits::new(Size::ZERO, bounds);

    layout::flex::resolve(
      layout::flex::Axis::Vertical,
      renderer,
      &limits,
      Fill,
      Fill,
      10.into(),
      10.0,
      Alignment::Center,
      self.toasts,
      self.state,
    )
    .translate(Vector::new(self.position.x, self.position.y))
  }

  fn update(
    &mut self,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    renderer: &Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
  ) {
    if let Event::Window(window::Event::RedrawRequested(now)) = &event {
      for id in self.toast_ids.iter().copied() {
        if let Some(instant) = self.instants.get(&id) {
          let remaining = time::seconds(self.timeout_secs).saturating_sub(instant.elapsed());
          if remaining == Duration::ZERO {
            self.instants.remove(&id);
            shell.publish((self.on_close)(id));
          } else {
            shell.request_redraw_at(*now + remaining);
          }
        }
      }
    }

    let viewport = layout.bounds();

    for (((child, state), layout), id) in self
      .toasts
      .iter_mut()
      .zip(self.state.iter_mut())
      .zip(layout.children())
      .zip(self.toast_ids.iter().copied())
    {
      let mut local_messages = vec![];
      let mut local_shell = Shell::new(shell.waker().clone(), &mut local_messages);

      child.as_widget_mut().update(
        state,
        event,
        layout,
        cursor,
        renderer,
        clipboard,
        &mut local_shell,
        &viewport,
      );

      if !local_shell.is_empty() {
        // toast 有交互后停止自动关闭计时
        self.instants.remove(&id);
      }

      shell.merge(local_shell, std::convert::identity);
    }
  }

  fn draw(
    &self,
    renderer: &mut Renderer,
    theme: &Theme,
    style: &renderer::Style,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
  ) {
    let viewport = layout.bounds();

    for ((child, state), layout) in self.toasts.iter().zip(self.state.iter()).zip(layout.children()) {
      child
        .as_widget()
        .draw(state, renderer, theme, style, layout, cursor, &viewport);
    }
  }

  fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn widget::Operation) {
    operation.container(None, layout.bounds());
    operation.traverse(&mut |operation| {
      self
        .toasts
        .iter_mut()
        .zip(self.state.iter_mut())
        .zip(layout.children())
        .for_each(|((child, state), layout)| {
          child.as_widget_mut().operate(state, layout, renderer, operation);
        });
    });
  }

  fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &Renderer) -> mouse::Interaction {
    self
      .toasts
      .iter()
      .zip(self.state.iter())
      .zip(layout.children())
      .map(|((child, state), layout)| {
        child
          .as_widget()
          .mouse_interaction(state, layout, cursor, &self.viewport, renderer)
          .max(if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Idle
          } else {
            Default::default()
          })
      })
      .max()
      .unwrap_or_default()
  }
}

impl<'a, Message> From<Manager<'a, Message>> for Element<'a, Message>
where
  Message: 'a,
{
  fn from(manager: Manager<'a, Message>) -> Self {
    Element::new(manager)
  }
}

fn styled(pair: theme::palette::Pair) -> container::Style {
  container::Style {
    background: Some(pair.color.into()),
    text_color: pair.text.into(),
    border: border::Border {
      radius: 4.0.into(),
      width: 1.0,
      color: color!(0xeeeff0),
    },
    shadow: Shadow {
      color: iced::Color::from_rgba8(37, 41, 49, 0.08),
      offset: Vector::new(0.0, 4.0),
      blur_radius: 16.0,
    },
    ..Default::default()
  }
}

fn success(_: &Theme) -> container::Style {
  styled(theme::palette::Pair::new(Color::WHITE, color!(0x252931)))
}

fn info(_: &Theme) -> container::Style {
  styled(theme::palette::Pair::new(Color::WHITE, color!(0x252931)))
}

fn warn(_: &Theme) -> container::Style {
  styled(theme::palette::Pair::new(Color::WHITE, color!(0x252931)))
}

fn danger(_: &Theme) -> container::Style {
  styled(theme::palette::Pair::new(Color::WHITE, color!(0x252931)))
}

#[derive(Debug)]
pub struct GlobalToast {
  toasts: BTreeMap<ToastId, Toast>,
  next_id: ToastId,
  max_toasts: usize,
  status_icon: StatusIcon,
}

#[derive(Debug)]
pub enum ToastMessage {
  Show { body: String, status: Status },
  Close(ToastId),
}

pub enum ToastAction {
  None,
}

impl GlobalToast {
  pub fn new() -> Self {
    Self::with_max_toasts(DEFAULT_MAX_TOASTS)
  }

  pub fn with_max_toasts(max_toasts: usize) -> Self {
    Self {
      toasts: BTreeMap::new(),
      next_id: 1,
      max_toasts,
      status_icon: StatusIcon::new(),
    }
  }

  pub fn set_max_toasts(&mut self, max_toasts: usize) {
    self.max_toasts = max_toasts;
    self.trim_excess_toasts();
  }
}

impl GlobalToast {
  fn trim_excess_toasts(&mut self) {
    while self.toasts.len() > self.max_toasts {
      let Some(oldest_toast_id) = self.toasts.keys().next().copied() else {
        break;
      };
      self.toasts.remove(&oldest_toast_id);
    }
  }

  pub fn update(&mut self, message: ToastMessage) -> ToastAction {
    match message {
      ToastMessage::Show { body, status } => {
        let mut id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        // 极端情况下 id 冲突时向后寻找一个空位，确保 key 唯一
        while self.toasts.contains_key(&id) {
          id = id.saturating_add(1);
        }
        self.toasts.insert(id, Toast { body, status });
        self.trim_excess_toasts();
        ToastAction::None
      }
      ToastMessage::Close(id) => {
        self.toasts.remove(&id);
        ToastAction::None
      }
    }
  }

  pub fn view<'a, Message>(
    &'a self,
    content: impl Into<Element<'a, Message>>,
    on_close: impl Fn(ToastId) -> Message + 'a,
    timeout_secs: u64,
  ) -> Element<'a, Message>
  where
    Message: 'a,
  {
    Manager::new(
      content,
      self.toasts.iter().map(|(id, toast)| (*id, toast)),
      &self.status_icon,
      on_close,
    )
    .timeout(timeout_secs)
    .into()
  }
}

#[cfg(test)]
mod tests {
  use super::{DEFAULT_MAX_TOASTS, GlobalToast, Status, ToastMessage};

  #[test]
  fn test_show_generates_unique_toast_ids() {
    let mut global_toast = GlobalToast::new();

    global_toast.update(ToastMessage::Show {
      body: "first".to_string(),
      status: Status::Info,
    });
    global_toast.update(ToastMessage::Show {
      body: "second".to_string(),
      status: Status::Success,
    });

    let ids = global_toast.toasts.keys().copied().collect::<Vec<_>>();
    assert_eq!(ids, vec![1, 2]);
  }

  #[test]
  fn test_close_removes_toast_by_id() {
    let mut global_toast = GlobalToast::new();

    global_toast.update(ToastMessage::Show {
      body: "first".to_string(),
      status: Status::Info,
    });
    global_toast.update(ToastMessage::Show {
      body: "second".to_string(),
      status: Status::Success,
    });

    global_toast.update(ToastMessage::Close(1));

    assert_eq!(global_toast.toasts.len(), 1);
    assert!(!global_toast.toasts.contains_key(&1));
    assert!(global_toast.toasts.contains_key(&2));
  }

  #[test]
  fn test_default_max_toasts_is_ten() {
    let global_toast = GlobalToast::new();

    assert_eq!(global_toast.max_toasts, DEFAULT_MAX_TOASTS);
  }

  #[test]
  fn test_show_removes_oldest_toast_when_exceeding_max_toasts() {
    let mut global_toast = GlobalToast::with_max_toasts(2);

    global_toast.update(ToastMessage::Show {
      body: "first".to_string(),
      status: Status::Info,
    });
    global_toast.update(ToastMessage::Show {
      body: "second".to_string(),
      status: Status::Warn,
    });
    global_toast.update(ToastMessage::Show {
      body: "third".to_string(),
      status: Status::Success,
    });

    let ids = global_toast.toasts.keys().copied().collect::<Vec<_>>();
    assert_eq!(ids, vec![2, 3]);
    assert_eq!(global_toast.toasts.len(), 2);
  }

  #[test]
  fn test_set_max_toasts_trims_existing_toasts() {
    let mut global_toast = GlobalToast::with_max_toasts(3);

    global_toast.update(ToastMessage::Show {
      body: "first".to_string(),
      status: Status::Info,
    });
    global_toast.update(ToastMessage::Show {
      body: "second".to_string(),
      status: Status::Warn,
    });
    global_toast.update(ToastMessage::Show {
      body: "third".to_string(),
      status: Status::Success,
    });

    global_toast.set_max_toasts(1);

    let ids = global_toast.toasts.keys().copied().collect::<Vec<_>>();
    assert_eq!(ids, vec![3]);
    assert_eq!(global_toast.toasts.len(), 1);
  }
}

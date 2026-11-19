use lintel_iced_widgets::ellipsis_text::EllipsisText;
use lintel_lib_iced::{
  self as iced, Alignment, Color, Element, Length, Theme,
  widget::{column, container, row, text},
};

pub fn main() -> iced::Result {
  iced::application(App::new, App::update, App::view)
    .theme(App::theme)
    .run()
}

#[derive(Debug, Clone)]
enum Message {}

struct App;

impl App {
  fn new() -> Self {
    Self
  }

  fn update(&mut self, _message: Message) {}

  fn theme(&self) -> Theme {
    Theme::Light
  }

  fn view(&self) -> Element<'_, Message> {
    // 这个 example 主要覆盖三件事：
    // 1. auto_fit 根据容器宽度自动决定是否省略
    // 2. align_x 控制省略后文本的对齐方式
    // 3. EllipsisText 可直接放进 row / column 等普通布局中使用
    let long_text = "这是一段很长很长的文本，用于演示 EllipsisText 组件在不同宽度和对齐方式下的省略效果";

    container(
      column![
        text("EllipsisText 示例").size(20),
        text("下面几组示例使用同一段长文案，但通过不同容器宽度和对齐方式，展示 auto_fit 的行为。"),
        text("默认左对齐"),
        container(
          EllipsisText::new(long_text, 18)
            .auto_fit()
            .size(14)
            .width(Length::Fill)
            .color(Color::from_rgb8(0x1D, 0x21, 0x29)),
        )
        .width(260),
        text("居中对齐"),
        container(
          EllipsisText::new(long_text, 14)
            .auto_fit()
            .size(14)
            .width(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .color(Color::from_rgb8(0x19, 0x66, 0xFF)),
        )
        .width(260),
        text("右对齐"),
        container(
          EllipsisText::new(long_text, 12)
            .auto_fit()
            .size(14)
            .width(Length::Fill)
            .align_x(iced::alignment::Horizontal::Right)
            .color(Color::from_rgb8(0xF5, 0x3F, 0x3F)),
        )
        .width(260),
        text("一行混排"),
        text("这个场景最接近搜索筛选、列表标题、属性值摘要等需要与其他控件共行展示的用法。"),
        row![
          text("标签："),
          EllipsisText::new("子账号分流设置方法，非常非常长的标题", 10)
            .auto_fit()
            .size(14)
            .width(180),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
      ]
      .spacing(16)
      .width(Length::Fill),
    )
    .padding(24)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
  }
}

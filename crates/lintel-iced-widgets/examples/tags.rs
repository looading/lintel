use lintel_iced_widgets::tags::{
  Tag, TagCategory, TagItem, TagPreset, TagSize, TagType, TagValue, Tags, TagsDirection,
};
use lintel_lib_iced::{
  self as iced, Alignment, Color, Element, Length, Padding, Task,
  widget::{button, column, container, row, scrollable, slider, text},
};

pub fn main() -> iced::Result {
  let _ = lintel_lib_env::LintelEnv::init_for_test();
  iced::application(App::new, App::update, App::view).run()
}

#[derive(Debug, Clone)]
enum Message {
  ToggleDirection,
  ToggleCloseable,
  SetSpacing(f32),
  RemoveScenarioTag(String),
  RemoveDenseTag(String),
  RestoreScenarioTags,
  RestoreDenseTags,
  RemoveSingleCustom,
  RestoreSingleCustom,
}

struct App {
  direction: TagsDirection,
  closeable: bool,
  spacing: f32,
  scenario_tags: Vec<TagValue>,
  dense_tags: Vec<TagValue>,
  size_tags: Vec<TagValue>,
  custom_tag: Option<TagValue>,
}

impl App {
  fn new() -> Self {
    Self {
      direction: TagsDirection::Horizontal,
      closeable: true,
      spacing: 6.0,
      scenario_tags: Self::scenario_tags(),
      dense_tags: Self::dense_tags(),
      size_tags: vec![
        TagValue::new("small", "Small"),
        TagValue::new("normal", "Normal"),
        TagValue::new("large", "Large"),
        TagValue::new("custom", "Custom 18"),
      ],
      custom_tag: Some(TagValue::new("custom", "自定义标签")),
    }
  }

  fn scenario_tags() -> Vec<TagValue> {
    // 这一组用于演示：不同 TagType + 不同 TagCategory 的组合效果，
    // 同时也方便验证 closeable / spacing / direction 的交互行为。
    vec![
      TagValue::new("info", "Info"),
      TagValue::new("error", "Error"),
      TagValue::new("warn", "Warn"),
      TagValue::new("cancel", "Cancel"),
      TagValue::new("default", "Default"),
      TagValue::new("custom", "Custom"),
    ]
  }

  fn dense_tags() -> Vec<TagValue> {
    // 这一组更贴近搜索 / 筛选场景：文本更长，便于观察密集排布时的视觉效果。
    vec![
      TagValue::new("question", "工作区权限设置方法"),
      TagValue::new("manual", "设置页面入口在哪"),
      TagValue::new("status", "后台任务进度通知"),
    ]
  }

  fn scenario_item(tag: TagValue) -> TagItem {
    let tag_type = match tag.key.as_str() {
      "info" => TagType::Info,
      "error" => TagType::Error,
      "warn" => TagType::Warn,
      "cancel" => TagType::Cancel,
      "custom" => TagType::Custom(TagPreset {
        color: Color::from_rgb8(0x72, 0x2E, 0xD1),
      }),
      _ => TagType::Default,
    };
    let category = match tag.key.as_str() {
      "info" | "custom" => TagCategory::Strong,
      "error" | "warn" => TagCategory::Normal,
      _ => TagCategory::Weak,
    };

    TagItem {
      value: tag,
      tag_type,
      category,
    }
  }

  fn dense_item(tag: TagValue) -> TagItem {
    TagItem {
      value: tag,
      tag_type: TagType::Default,
      category: TagCategory::Normal,
    }
  }

  fn type_matrix_row(category: TagCategory) -> Vec<TagItem> {
    vec![
      TagItem::new("info", "Info", TagType::Info, category),
      TagItem::new("error", "Error", TagType::Error, category),
      TagItem::new("warn", "Warn", TagType::Warn, category),
      TagItem::new("cancel", "Cancel", TagType::Cancel, category),
      TagItem::new("default", "Default", TagType::Default, category),
      TagItem::new(
        "custom",
        "Custom",
        TagType::Custom(TagPreset {
          color: Color::from_rgb8(0x7B, 0x61, 0xFF),
        }),
        category,
      ),
    ]
  }

  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::ToggleDirection => {
        self.direction = match self.direction {
          TagsDirection::Horizontal => TagsDirection::Vertical,
          TagsDirection::Vertical => TagsDirection::Horizontal,
        };
        Task::none()
      }
      Message::ToggleCloseable => {
        self.closeable = !self.closeable;
        Task::none()
      }
      Message::SetSpacing(spacing) => {
        self.spacing = spacing;
        Task::none()
      }
      Message::RemoveScenarioTag(key) => {
        self.scenario_tags.retain(|tag| tag.key != key);
        Task::none()
      }
      Message::RemoveDenseTag(key) => {
        self.dense_tags.retain(|tag| tag.key != key);
        Task::none()
      }
      Message::RestoreScenarioTags => {
        self.scenario_tags = Self::scenario_tags();
        Task::none()
      }
      Message::RestoreDenseTags => {
        self.dense_tags = Self::dense_tags();
        Task::none()
      }
      Message::RemoveSingleCustom => {
        self.custom_tag = None;
        Task::none()
      }
      Message::RestoreSingleCustom => {
        self.custom_tag = Some(TagValue::new("custom", "自定义标签"));
        Task::none()
      }
    }
  }

  fn view(&self) -> Element<'_, Message> {
    let direction_label = match self.direction {
      TagsDirection::Horizontal => "切换为纵向",
      TagsDirection::Vertical => "切换为横向",
    };
    let closeable_label = if self.closeable {
      "关闭删除按钮"
    } else {
      "开启删除按钮"
    };

    let tags_preview: Element<'_, Message> = Tags::new(
      self
        .scenario_tags
        .iter()
        .cloned()
        .map(Self::scenario_item)
        .collect::<Vec<_>>(),
    )
    .direction(self.direction)
    .spacing(self.spacing)
    .closeable(self.closeable)
    .on_close(|tag| Message::RemoveScenarioTag(tag.value.key.clone()))
    .into();

    let dense_tags: Element<'_, Message> = Tags::new(
      self
        .dense_tags
        .iter()
        .cloned()
        .map(Self::dense_item)
        .collect::<Vec<_>>(),
    )
    .spacing(4.0)
    .tag_padding(Padding {
      top: 2.0,
      right: 8.0,
      bottom: 2.0,
      left: 8.0,
    })
    .closeable(self.closeable)
    .on_close(|tag| Message::RemoveDenseTag(tag.value.key.clone()))
    .into();

    let strong_types: Element<'_, Message> = Tags::new(Self::type_matrix_row(TagCategory::Strong))
      .spacing(8.0)
      .into();
    let normal_types: Element<'_, Message> = Tags::new(Self::type_matrix_row(TagCategory::Normal))
      .spacing(8.0)
      .into();
    let weak_types: Element<'_, Message> = Tags::new(Self::type_matrix_row(TagCategory::Weak)).spacing(8.0).into();

    let single_custom_tag: Element<'_, Message> = if let Some(custom_tag) = self.custom_tag.as_ref() {
      Tag::new(
        custom_tag.clone(),
        TagType::Custom(TagPreset {
          color: Color::from_rgb8(0x00, 0x8A, 0x6A),
        }),
        TagCategory::Normal,
      )
      .closeable(self.closeable)
      .on_close(Message::RemoveSingleCustom)
      .into()
    } else {
      button("恢复自定义标签").on_press(Message::RestoreSingleCustom).into()
    };

    let size_preview: Element<'_, Message> = row![
      Tag::new(self.size_tags[0].clone(), TagType::Default, TagCategory::Weak).size(TagSize::Small),
      Tag::new(self.size_tags[1].clone(), TagType::Default, TagCategory::Normal).size(TagSize::Normal),
      Tag::new(self.size_tags[2].clone(), TagType::Default, TagCategory::Strong).size(TagSize::Large),
      Tag::new(self.size_tags[3].clone(), TagType::Default, TagCategory::Normal).size(TagSize::Custom(18)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into();

    let vertical_showcase: Element<'_, Message> = Tags::new(vec![
      TagItem::new("queue", "队列中", TagType::Info, TagCategory::Weak),
      TagItem::new("reply", "待回复", TagType::Warn, TagCategory::Normal),
      TagItem::new("risk", "高优先级", TagType::Error, TagCategory::Strong),
    ])
    .direction(TagsDirection::Vertical)
    .spacing(8.0)
    .size(TagSize::Normal)
    .into();

    let mixed_size_showcase: Element<'_, Message> = column![
      row![
        Tag::new(TagValue::new("s", "Weak Small"), TagType::Info, TagCategory::Weak).size(TagSize::Small),
        Tag::new(TagValue::new("n", "Normal"), TagType::Warn, TagCategory::Normal).size(TagSize::Normal),
        Tag::new(TagValue::new("l", "Strong Large"), TagType::Error, TagCategory::Strong).size(TagSize::Large),
      ]
      .spacing(8)
      .align_y(Alignment::Center),
      row![
        Tag::new(
          TagValue::new("c1", "Custom Purple"),
          TagType::Custom(TagPreset {
            color: Color::from_rgb8(0x7B, 0x61, 0xFF),
          }),
          TagCategory::Normal,
        ),
        Tag::new(
          TagValue::new("c2", "Custom Mint"),
          TagType::Custom(TagPreset {
            color: Color::from_rgb8(0x00, 0xA8, 0x6B),
          }),
          TagCategory::Weak,
        ),
      ]
      .spacing(8)
      .align_y(Alignment::Center),
    ]
    .spacing(8)
    .into();

    container(
      scrollable(
        column![
          text("Tags 组件示例").size(20),
          text("这个 example 分成三块：交互式批量标签、视觉矩阵覆盖、单个 Tag 能力预览。"),
          row![
            button(direction_label).on_press(Message::ToggleDirection),
            button(closeable_label).on_press(Message::ToggleCloseable),
            button("恢复多类型标签").on_press(Message::RestoreScenarioTags),
            button("恢复搜索标签").on_press(Message::RestoreDenseTags),
            text(format!("当前间距：{:.0}", self.spacing)),
            slider(0.0..=20.0, self.spacing, Message::SetSpacing).step(1.0),
          ]
          .spacing(12)
          .align_y(Alignment::Center),
          text("交互式多类型标签"),
          text("可切换横向/纵向排列、删除按钮、标签间距；删除后可通过“恢复多类型标签”回填。"),
          tags_preview,
          text("搜索场景标签"),
          text("更接近搜索筛选和 query chips 的真实文案长度。"),
          dense_tags,
          text("类型 x Category 覆盖"),
          text("同一套 TagType 分别放到 Strong / Normal / Weak 中，对比颜色和层级变化。"),
          text("Strong"),
          strong_types,
          text("Normal"),
          normal_types,
          text("Weak"),
          weak_types,
          text("Tag 尺寸"),
          text("用于观察 Small / Normal / Large / Custom 的高度与内边距差异。"),
          size_preview,
          text("混合尺寸与自定义颜色"),
          text("展示自定义色值 + 不同尺寸混排的效果。"),
          mixed_size_showcase,
          text("纵向排列"),
          text("适合状态堆叠或表单摘要等纵向信息流。"),
          vertical_showcase,
          text("单个自定义标签"),
          text("演示单个 Tag 的 close 行为；删除后可单独恢复。"),
          single_custom_tag,
        ]
        .spacing(16)
        .width(Length::Fill),
      )
      .width(Length::Fill)
      .height(Length::Fill),
    )
    .padding(24)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
  }
}

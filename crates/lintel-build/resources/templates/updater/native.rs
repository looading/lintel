//! 复制到已生成工程的 src/main.rs，再配置更新源和公钥。
use lintel::iced::{
  Element, Task,
  widget::{button, center, column, text},
  window,
};
use lintel_updater::Updater;

const UPDATE_FEED: &str = "https://updates.example.test/latest.json";
const UPDATE_PUBLIC_KEY: &str = "REPLACE_WITH_BASE64_ED25519_PUBLIC_KEY";

#[derive(Default)]
struct State {
  status: String,
  busy: bool,
}

#[derive(Debug, Clone)]
enum Message {
  Install,
  Finished(Result<bool, String>),
}

fn main() -> lintel::Result<()> {
  lintel::application(State::default, update, view)
    .id(concat!("dev.example.", env!("CARGO_PKG_NAME")))
    .title("Lintel Updater")
    .run()
}

fn update(state: &mut State, message: Message) -> Task<Message> {
  match message {
    Message::Install if !state.busy => {
      state.busy = true;
      state.status = "正在检查和准备更新…".into();
      Task::perform(
        async {
          let result: lintel::Result<bool> = async {
            let updater = Updater::for_current_app(UPDATE_FEED, UPDATE_PUBLIC_KEY)?;
            let Some(update) = updater.check().await? else {
              return Ok(false);
            };
            let prepared = update
              .download(|p| eprintln!("download {}/{}", p.downloaded, p.total))
              .await?
              .prepare()
              .await?;
            prepared.install_on_exit(true).await?;
            Ok(true)
          }
          .await;
          result.map_err(|error| format!("{error:#}"))
        },
        Message::Finished,
      )
    }
    Message::Finished(Ok(true)) => {
      // 使用原有窗口关闭路径，让运行时完成资源回收后自然退出。
      window::oldest().then(|id| id.map_or_else(Task::none, window::close))
    }
    Message::Finished(Ok(false)) => {
      state.busy = false;
      state.status = "当前已是最新版本".into();
      Task::none()
    }
    Message::Finished(Err(error)) => {
      eprintln!("update failed: {error}");
      state.busy = false;
      state.status = "更新失败，请稍后重试".into();
      Task::none()
    }
    _ => Task::none(),
  }
}

fn view(state: &State) -> Element<'_, Message> {
  center(
    column![
      text(format!("Version {}", env!("CARGO_PKG_VERSION"))),
      text(&state.status),
      button("检查并安装更新").on_press_maybe((!state.busy).then_some(Message::Install)),
    ]
    .spacing(12),
  )
  .into()
}

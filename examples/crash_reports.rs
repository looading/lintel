//! Local snapshots and optional sending through the crash report plugin.
use std::{path::PathBuf, sync::Arc};

use lintel::{
  AppContextApi, TaskScope,
  crash::CrashConfig,
  crash_reports::{
    CrashReports, CrashReportsConfig, FatalCapture, ReportId, ReportQuery, ReportSourceConfig, ReportSummary,
    SourceKind, UploadMode, UploadTarget,
  },
  iced::{
    Element, Task,
    widget::{button, column, row, text},
  },
};

const APP_ID: &str = "dev.lintel.crash-reports";

struct State {
  service: Arc<CrashReports>,
  reports: Vec<ReportSummary>,
  message: String,
}

#[derive(Debug, Clone)]
enum Message {
  Capture,
  Refresh,
  Send(ReportId),
  Finished(std::result::Result<Vec<ReportSummary>, String>),
}

#[derive(Clone, Copy)]
enum StartupAction {
  Capture,
  List,
}

fn update(state: &mut State, message: Message) -> Task<Message> {
  let service = state.service.clone();
  match message {
    Message::Capture | Message::Refresh | Message::Send(_) => Task::perform(
      async move {
        match message {
          Message::Capture => service.capture_now().await.map(|_| ()),
          Message::Send(id) => service.request_upload(id).await.map(|_| ()),
          _ => Ok(()),
        }
        .map_err(|error| error.to_string())?;
        service
          .list_reports(ReportQuery::default())
          .await
          .map_err(|error| error.to_string())
      },
      Message::Finished,
    ),
    Message::Finished(result) => {
      match result {
        Ok(reports) => {
          state.reports = reports;
          state.message.clear();
        }
        Err(error) => state.message = error,
      }
      Task::none()
    }
  }
}

fn view(state: &State) -> Element<'_, Message> {
  let mut content = column![
    row![
      button("Capture snapshot").on_press(Message::Capture),
      button("Refresh").on_press(Message::Refresh)
    ]
    .spacing(8),
    text(&state.message),
  ]
  .padding(16)
  .spacing(8);
  for report in &state.reports {
    content = content.push(
      row![
        text(format!(
          "{:?} · {} bytes · {:?}",
          report.kind, report.size_bytes, report.status
        )),
        button("Send").on_press(Message::Send(report.id))
      ]
      .spacing(8),
    );
  }
  content.into()
}

fn main() -> lintel::Result<()> {
  let mut args = std::env::args_os().skip(1);
  let root = args.next().map(PathBuf::from).ok_or_else(|| {
    anyhow::anyhow!("usage: crash_reports /absolute/data [--crash-helper PATH] [--snapshot-helper PATH] [--uploader PATH --endpoint HTTPS_URL] [--automatic-upload] [--import-cef PATH] [--capture-and-exit | --list-and-exit]")
  })?;
  let mut config = CrashReportsConfig::new(APP_ID, env!("CARGO_PKG_VERSION"), root.join("reports"));
  let mut startup_action = None;
  while let Some(option) = args.next() {
    match option.to_str() {
      Some("--automatic-upload") => config.reports.policy.mode = UploadMode::Automatic,
      Some("--capture-and-exit") => startup_action = Some(StartupAction::Capture),
      Some("--list-and-exit") => startup_action = Some(StartupAction::List),
      Some("--import-cef") => {
        config.reports.sources.push(ReportSourceConfig {
          id: "cef-history".into(),
          directory: args
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing --import-cef directory"))?
            .into(),
          kind: SourceKind::Cef,
        });
      }
      Some("--crash-helper") => {
        let helper = args
          .next()
          .ok_or_else(|| anyhow::anyhow!("missing --crash-helper path"))?;
        config.fatal_capture = FatalCapture::Native(CrashConfig::new(root.join("native"), helper));
      }
      Some("--snapshot-helper") => {
        config.snapshot_helper = Some(
          args
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing --snapshot-helper path"))?
            .into(),
        );
      }
      Some("--uploader") => {
        config.uploader = Some(
          args
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing --uploader path"))?
            .into(),
        );
      }
      Some("--endpoint") => {
        let endpoint = args
          .next()
          .and_then(|value| value.into_string().ok())
          .ok_or_else(|| anyhow::anyhow!("missing HTTPS upload endpoint"))?;
        config.reports.upload = Some(UploadTarget::new(endpoint));
      }
      _ => anyhow::bail!("unrecognized crash report example option"),
    }
  }
  lintel::application_with_context(
    move |context| {
      let service = context
        .get_service::<CrashReports>()
        .ok_or_else(|| anyhow::anyhow!("crash report service unavailable"))?;
      if let Some(action) = startup_action {
        let reports = service.clone();
        let app = context.clone();
        context.spawn_task(TaskScope::Application, async move {
          let result = match action {
            StartupAction::Capture => reports.capture_now().await.map(|report| println!("{}", report.id)),
            StartupAction::List => reports
              .list_reports(ReportQuery::default())
              .await
              .map(|reports| println!("{}", reports.len())),
          };
          let code = match result {
            Ok(()) => 0,
            Err(error) => {
              eprintln!("{error}");
              1
            }
          };
          let _ = app.exit(code);
        })?;
      }
      Ok(State {
        service,
        reports: Vec::new(),
        message: String::new(),
      })
    },
    update,
    view,
  )
  .id(APP_ID)
  .title("Crash reports")
  .crash_reports(config)
  .run()
}

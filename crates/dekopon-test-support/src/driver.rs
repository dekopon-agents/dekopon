use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::Notify;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverCall {
    Reply {
        text: String,
        images: Vec<usize>,
    },
    Typing {
        target: String,
    },
    Status {
        target: String,
        status: &'static str,
    },
    Progress(ProgressCall),
    Stream(StreamCall),
    Reaction {
        target: String,
        present: bool,
    },
    CancelAck {
        subject: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressCall {
    Post {
        target: String,
        text: String,
        cancel: bool,
    },
    Edit {
        message: String,
        text: String,
        cancel: bool,
    },
    Delete {
        message: String,
    },
    Finalize {
        message: String,
        text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamCall {
    Show {
        target: String,
        message: Option<String>,
        text: String,
        truncated: bool,
        cancel: bool,
    },
    Finalize {
        message: String,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    Response,
    RateLimited,
    Closed,
}

#[derive(Debug)]
struct Injector {
    calls: AtomicUsize,
    plan: Mutex<Vec<(usize, FailureKind)>>,
    from: Mutex<Option<(usize, FailureKind)>>,
    called: Notify,
}

impl Default for Injector {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            plan: Mutex::new(Vec::new()),
            from: Mutex::new(None),
            called: Notify::new(),
        }
    }
}

impl Injector {
    fn charge(&self) -> Option<FailureKind> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.called.notify_one();
        if let Some((first, kind)) = *self.from.lock().expect("injection plan")
            && index >= first
        {
            return Some(kind);
        }
        self.plan
            .lock()
            .expect("injection plan")
            .iter()
            .find_map(|(at, kind)| (*at == index).then_some(*kind))
    }
}

macro_rules! injected {
    ($object:ty) => {
        impl $object {
            #[must_use]
            pub fn failing_call(self, index: usize, kind: FailureKind) -> Self {
                self.injector
                    .plan
                    .lock()
                    .expect("injection plan")
                    .push((index, kind));
                self
            }

            #[must_use]
            pub fn failing_from(self, index: usize, kind: FailureKind) -> Self {
                *self.injector.from.lock().expect("injection plan") = Some((index, kind));
                self
            }

            #[must_use]
            pub fn calls(&self) -> usize {
                self.injector.calls.load(Ordering::SeqCst)
            }

            pub async fn wait_for_calls(&self, count: usize) {
                while self.injector.calls.load(Ordering::SeqCst) < count {
                    self.injector.called.notified().await;
                }
            }

            #[must_use]
            pub fn charge(&self) -> Option<FailureKind> {
                self.injector.charge()
            }
        }
    };
}

type Log = Arc<Mutex<Vec<DriverCall>>>;

#[derive(Debug)]
pub struct RecordingTyping {
    log: Log,
    injector: Injector,
    interval: Duration,
}

injected!(RecordingTyping);

impl RecordingTyping {
    #[must_use]
    pub fn renew_every(&self) -> Duration {
        self.interval
    }

    pub fn record(&self, target: String) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Typing { target });
    }
}

#[derive(Debug)]
pub struct RecordingStatus {
    log: Log,
    injector: Injector,
}

injected!(RecordingStatus);

impl RecordingStatus {
    pub fn record(&self, target: String, status: &'static str) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Status { target, status });
    }
}

#[derive(Debug)]
pub struct RecordingProgress {
    log: Log,
    injector: Injector,
    max_chars: usize,
    min_edit_interval: Duration,
}

injected!(RecordingProgress);

impl RecordingProgress {
    #[must_use]
    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    #[must_use]
    pub fn min_edit_interval(&self) -> Duration {
        self.min_edit_interval
    }

    pub fn record(&self, call: ProgressCall) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Progress(call));
    }
}

#[derive(Debug)]
pub struct RecordingStream {
    log: Log,
    injector: Injector,
    max_chars: usize,
    min_interval: Duration,
}

injected!(RecordingStream);

impl RecordingStream {
    #[must_use]
    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    #[must_use]
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    pub fn record(&self, call: StreamCall) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Stream(call));
    }
}

#[derive(Debug)]
pub struct RecordingReaction {
    log: Log,
    injector: Injector,
}

injected!(RecordingReaction);

impl RecordingReaction {
    pub fn record(&self, target: String, present: bool) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Reaction { target, present });
    }
}

#[derive(Debug)]
pub struct RecordingCancelButton {
    log: Log,
    injector: Injector,
}

injected!(RecordingCancelButton);

impl RecordingCancelButton {
    pub fn record(&self, subject: String) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::CancelAck { subject });
    }
}

#[derive(Debug)]
pub struct RecordingDriver {
    log: Log,
    replies: Injector,
    typing: Option<RecordingTyping>,
    status: Option<RecordingStatus>,
    progress: Option<RecordingProgress>,
    stream: Option<RecordingStream>,
    reaction: Option<RecordingReaction>,
    cancel_button: Option<RecordingCancelButton>,
}

impl Default for RecordingDriver {
    fn default() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            replies: Injector::default(),
            typing: None,
            status: None,
            progress: None,
            stream: None,
            reaction: None,
            cancel_button: None,
        }
    }
}

impl RecordingDriver {
    #[must_use]
    pub fn with_typing(mut self, interval: Duration) -> Self {
        self.typing = Some(RecordingTyping {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
            interval,
        });
        self
    }

    #[must_use]
    pub fn with_status(mut self) -> Self {
        self.status = Some(RecordingStatus {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
        });
        self
    }

    #[must_use]
    pub fn with_progress(mut self, max_chars: usize, min_edit_interval: Duration) -> Self {
        self.progress = Some(RecordingProgress {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
            max_chars,
            min_edit_interval,
        });
        self
    }

    #[must_use]
    pub fn with_stream(mut self, max_chars: usize, min_interval: Duration) -> Self {
        self.stream = Some(RecordingStream {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
            max_chars,
            min_interval,
        });
        self
    }

    #[must_use]
    pub fn with_reaction(mut self) -> Self {
        self.reaction = Some(RecordingReaction {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
        });
        self
    }

    #[must_use]
    pub fn with_cancel_button(mut self) -> Self {
        self.cancel_button = Some(RecordingCancelButton {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
        });
        self
    }

    #[must_use]
    pub fn configure_progress(
        mut self,
        configure: impl FnOnce(RecordingProgress) -> RecordingProgress,
    ) -> Self {
        let progress = self
            .progress
            .take()
            .expect("the progress object is present");
        self.progress = Some(configure(progress));
        self
    }

    #[must_use]
    pub fn configure_stream(
        mut self,
        configure: impl FnOnce(RecordingStream) -> RecordingStream,
    ) -> Self {
        let stream = self.stream.take().expect("the stream object is present");
        self.stream = Some(configure(stream));
        self
    }

    #[must_use]
    pub fn configure_status(
        mut self,
        configure: impl FnOnce(RecordingStatus) -> RecordingStatus,
    ) -> Self {
        let status = self.status.take().expect("the status object is present");
        self.status = Some(configure(status));
        self
    }

    #[must_use]
    pub fn failing_replies_from(self, index: usize, kind: FailureKind) -> Self {
        *self.replies.from.lock().expect("injection plan") = Some((index, kind));
        self
    }

    #[must_use]
    pub fn charge_reply(&self) -> Option<FailureKind> {
        self.replies.charge()
    }

    pub fn record_reply(&self, text: String, images: Vec<usize>) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Reply { text, images });
    }

    /// Named apart from the trait accessor with the same job, since inherent methods win
    /// method-call lookup and the trait implementation would otherwise silently call itself.
    #[must_use]
    pub fn typing_object(&self) -> Option<&RecordingTyping> {
        self.typing.as_ref()
    }

    #[must_use]
    pub fn status_object(&self) -> Option<&RecordingStatus> {
        self.status.as_ref()
    }

    #[must_use]
    pub fn progress_object(&self) -> Option<&RecordingProgress> {
        self.progress.as_ref()
    }

    #[must_use]
    pub fn stream_object(&self) -> Option<&RecordingStream> {
        self.stream.as_ref()
    }

    #[must_use]
    pub fn reaction_object(&self) -> Option<&RecordingReaction> {
        self.reaction.as_ref()
    }

    #[must_use]
    pub fn cancel_button_object(&self) -> Option<&RecordingCancelButton> {
        self.cancel_button.as_ref()
    }

    #[must_use]
    pub fn calls(&self) -> Vec<DriverCall> {
        self.log.lock().expect("driver log").clone()
    }

    #[must_use]
    pub fn rendered(&self) -> Vec<String> {
        self.calls().into_iter().map(render).collect()
    }

    #[must_use]
    pub fn replies(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                DriverCall::Reply { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    #[must_use]
    pub fn image_bytes(&self) -> Vec<Vec<usize>> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                DriverCall::Reply { images, .. } => Some(images),
                _ => None,
            })
            .collect()
    }
}

fn render(call: DriverCall) -> String {
    match call {
        DriverCall::Reply { text, images } if images.is_empty() => format!("reply:{text}"),
        DriverCall::Reply { text, images } => format!("reply:{text}+{}", images.len()),
        DriverCall::Typing { .. } => "typing".to_owned(),
        DriverCall::Status { status, .. } => format!("status:{status}"),
        DriverCall::Progress(ProgressCall::Post { text, cancel, .. }) => {
            format!("progress.post:{text}{}", affordance(cancel))
        }
        DriverCall::Progress(ProgressCall::Edit { text, cancel, .. }) => {
            format!("progress.edit:{text}{}", affordance(cancel))
        }
        DriverCall::Progress(ProgressCall::Delete { .. }) => "progress.delete".to_owned(),
        DriverCall::Progress(ProgressCall::Finalize { text, .. }) => {
            format!("progress.finalize:{text}")
        }
        DriverCall::Stream(StreamCall::Show {
            text,
            truncated,
            cancel,
            ..
        }) => format!(
            "stream.show:{text}{}{}",
            if truncated { "+truncated" } else { "" },
            affordance(cancel)
        ),
        DriverCall::Stream(StreamCall::Finalize { text, .. }) => format!("stream.finalize:{text}"),
        DriverCall::Reaction { present, .. } => {
            format!("reaction:{}", if present { "set" } else { "cleared" })
        }
        DriverCall::CancelAck { subject } => format!("cancel.ack:{subject}"),
    }
}

fn affordance(cancel: bool) -> &'static str {
    if cancel { "+stop" } else { "" }
}

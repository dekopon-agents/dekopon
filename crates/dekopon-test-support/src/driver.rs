//! A chat driver that records every call and can be made to fail one object at a time.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::Notify;

/// What a driver was asked to do, in the order it was asked.
///
/// Coordinates and text arrive here already rendered to strings. The gateway's own liveness,
/// message-reference, and reply types are private to `dekopond`, and a shared fixture that
/// mirrored them would be a second definition of each; instead the gateway's own test module
/// implements the capability traits over these recorders and renders as it records. What is shared
/// is the part every suite would otherwise rewrite: the log, the per-object switches, and the
/// failure injection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverCall {
    /// `ChatDriver::reply`: the terminal answer and the size of each attachment on it.
    Reply {
        /// The answer text as the person would read it.
        text: String,
        /// One byte count per attachment, in the order the reply carried them.
        images: Vec<usize>,
    },
    /// A typing lease renewal.
    Typing {
        /// The rendered liveness target.
        target: String,
    },
    /// A native status change.
    Status {
        /// The rendered liveness target.
        target: String,
        /// `working` or `idle`.
        status: &'static str,
    },
    /// One call on the progress-message object.
    Progress(ProgressCall),
    /// One call on the text-stream object.
    Stream(StreamCall),
    /// A reaction set or cleared on the inbound message.
    Reaction {
        /// The rendered liveness target.
        target: String,
        /// Whether the reaction is now present.
        present: bool,
    },
    /// A cancel press acknowledged inside the service deadline.
    CancelAck {
        /// The subject whose press was acknowledged.
        subject: String,
    },
}

/// One call on the progress-message object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressCall {
    /// The first post, which is what returns the session's one message reference.
    Post {
        /// The rendered liveness target.
        target: String,
        /// The rendered progress text.
        text: String,
        /// Whether a cancel affordance was asked for.
        cancel: bool,
    },
    /// An edit of that message.
    Edit {
        /// The rendered message reference.
        message: String,
        /// The rendered progress text.
        text: String,
        /// Whether a cancel affordance was asked for.
        cancel: bool,
    },
    /// A delete of that message.
    Delete {
        /// The rendered message reference.
        message: String,
    },
    /// Turning that message into the answer in place.
    Finalize {
        /// The rendered message reference.
        message: String,
        /// The answer text.
        text: String,
    },
}

/// One call on the text-stream object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamCall {
    /// Cumulative text shown so far.
    Show {
        /// The rendered liveness target.
        target: String,
        /// The rendered message reference, absent on the first call.
        message: Option<String>,
        /// The cumulative text as the person would read it.
        text: String,
        /// Whether the policy had to truncate it.
        truncated: bool,
        /// Whether a cancel affordance was asked for.
        cancel: bool,
    },
    /// Turning the stream into the answer in place.
    Finalize {
        /// The rendered message reference.
        message: String,
        /// The answer text.
        text: String,
    },
}

/// How an injected failure should surface, chosen by the test and mapped by the gateway.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    /// The service answered something unusable.
    Response,
    /// The service asked for a cooldown.
    RateLimited,
    /// The connection is gone.
    Closed,
}

/// The switch every recording capability object shares: count the calls, fail the chosen ones.
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
    /// Charges one call and reports whether this one was chosen to fail.
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

/// Declares the failure switches every recording capability object carries.
macro_rules! injected {
    ($object:ty) => {
        impl $object {
            /// Fails the call at this zero-based index and no other.
            #[must_use]
            pub fn failing_call(self, index: usize, kind: FailureKind) -> Self {
                self.injector
                    .plan
                    .lock()
                    .expect("injection plan")
                    .push((index, kind));
                self
            }

            /// Fails this call and every later one, which is how a degraded rung is reached.
            #[must_use]
            pub fn failing_from(self, index: usize, kind: FailureKind) -> Self {
                *self.injector.from.lock().expect("injection plan") = Some((index, kind));
                self
            }

            /// How many calls this object has taken.
            #[must_use]
            pub fn calls(&self) -> usize {
                self.injector.calls.load(Ordering::SeqCst)
            }

            /// Resolves once this object has taken at least `count` calls.
            ///
            /// The count rather than the signal is what a test means: a driver that finished two
            /// calls before the test looked has satisfied "wait for two", and an announcement it
            /// slept through has not made that untrue.
            pub async fn wait_for_calls(&self, count: usize) {
                while self.injector.calls.load(Ordering::SeqCst) < count {
                    self.injector.called.notified().await;
                }
            }

            /// Charges one call and reports the failure the test asked for, if any.
            ///
            /// The gateway's own implementation calls this, records, and turns a returned kind
            /// into the transport error it stands for.
            #[must_use]
            pub fn charge(&self) -> Option<FailureKind> {
                self.injector.charge()
            }
        }
    };
}

/// Shared log every recorder writes into.
type Log = Arc<Mutex<Vec<DriverCall>>>;

/// The typing-lease object, present only when a test asked for it.
#[derive(Debug)]
pub struct RecordingTyping {
    log: Log,
    injector: Injector,
    interval: Duration,
}

injected!(RecordingTyping);

impl RecordingTyping {
    /// How often the policy is told to renew.
    #[must_use]
    pub fn renew_every(&self) -> Duration {
        self.interval
    }

    /// Records one renewal against a rendered target.
    pub fn record(&self, target: String) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Typing { target });
    }
}

/// The native-status object.
#[derive(Debug)]
pub struct RecordingStatus {
    log: Log,
    injector: Injector,
}

injected!(RecordingStatus);

impl RecordingStatus {
    /// Records one status change against a rendered target.
    pub fn record(&self, target: String, status: &'static str) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Status { target, status });
    }
}

/// The progress-message object, carrying the limits the policy reads off it.
#[derive(Debug)]
pub struct RecordingProgress {
    log: Log,
    injector: Injector,
    max_chars: usize,
    min_edit_interval: Duration,
}

injected!(RecordingProgress);

impl RecordingProgress {
    /// The character ceiling this surface advertises.
    #[must_use]
    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    /// The shortest interval between edits this surface advertises.
    #[must_use]
    pub fn min_edit_interval(&self) -> Duration {
        self.min_edit_interval
    }

    /// Records one call.
    pub fn record(&self, call: ProgressCall) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Progress(call));
    }
}

/// The text-stream object, carrying the limits the policy reads off it.
#[derive(Debug)]
pub struct RecordingStream {
    log: Log,
    injector: Injector,
    max_chars: usize,
    min_interval: Duration,
}

injected!(RecordingStream);

impl RecordingStream {
    /// The character ceiling this surface advertises.
    #[must_use]
    pub fn max_chars(&self) -> usize {
        self.max_chars
    }

    /// The shortest interval between renders this surface advertises.
    #[must_use]
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Records one call.
    pub fn record(&self, call: StreamCall) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Stream(call));
    }
}

/// The inbound-reaction object.
#[derive(Debug)]
pub struct RecordingReaction {
    log: Log,
    injector: Injector,
}

injected!(RecordingReaction);

impl RecordingReaction {
    /// Records one reaction change against a rendered target.
    pub fn record(&self, target: String, present: bool) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Reaction { target, present });
    }
}

/// The cancel-button object.
#[derive(Debug)]
pub struct RecordingCancelButton {
    log: Log,
    injector: Injector,
}

injected!(RecordingCancelButton);

impl RecordingCancelButton {
    /// Records one acknowledged press.
    pub fn record(&self, subject: String) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::CancelAck { subject });
    }
}

/// A driver whose capability objects are switched on per test, and each of which can be made to
/// fail on its own.
///
/// Absent means unimplemented, which is the whole point of the capability-object shape: a test for
/// "a transport with no progress surface still streams" builds a driver with `stream` on and
/// `progress` off, and the policy has nothing to consult beyond that. Failure injection is
/// per object because the rungs degrade independently — a Slack workspace can be rate-limiting
/// `chat.update` while reactions still work.
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
    /// A driver that can only reply, which is every transport's floor.
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
    /// Adds a typing lease renewed at `interval`.
    #[must_use]
    pub fn with_typing(mut self, interval: Duration) -> Self {
        self.typing = Some(RecordingTyping {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
            interval,
        });
        self
    }

    /// Adds a native status object.
    #[must_use]
    pub fn with_status(mut self) -> Self {
        self.status = Some(RecordingStatus {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
        });
        self
    }

    /// Adds an editable progress message advertising these limits.
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

    /// Adds a text stream advertising these limits.
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

    /// Adds an inbound reaction object.
    #[must_use]
    pub fn with_reaction(mut self) -> Self {
        self.reaction = Some(RecordingReaction {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
        });
        self
    }

    /// Adds a cancel button.
    #[must_use]
    pub fn with_cancel_button(mut self) -> Self {
        self.cancel_button = Some(RecordingCancelButton {
            log: Arc::clone(&self.log),
            injector: Injector::default(),
        });
        self
    }

    /// Replaces the object a test wants to configure, keeping the shared log.
    ///
    /// The builders above hand back the driver, so the object itself is never in the caller's
    /// hands to call `failing_from` on. These four give it back for exactly that, and each panics
    /// when the matching object was never switched on — a failure plan installed on an absent
    /// object would silently assert nothing.
    ///
    /// # Panics
    ///
    /// When the object being configured is absent.
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

    /// Configures the stream object; see [`Self::configure_progress`].
    ///
    /// # Panics
    ///
    /// When the stream object is absent.
    #[must_use]
    pub fn configure_stream(
        mut self,
        configure: impl FnOnce(RecordingStream) -> RecordingStream,
    ) -> Self {
        let stream = self.stream.take().expect("the stream object is present");
        self.stream = Some(configure(stream));
        self
    }

    /// Configures the status object; see [`Self::configure_progress`].
    ///
    /// # Panics
    ///
    /// When the status object is absent.
    #[must_use]
    pub fn configure_status(
        mut self,
        configure: impl FnOnce(RecordingStatus) -> RecordingStatus,
    ) -> Self {
        let status = self.status.take().expect("the status object is present");
        self.status = Some(configure(status));
        self
    }

    /// Fails `reply` from this call onward.
    #[must_use]
    pub fn failing_replies_from(self, index: usize, kind: FailureKind) -> Self {
        *self.replies.from.lock().expect("injection plan") = Some((index, kind));
        self
    }

    /// Charges one `reply` and reports the failure the test asked for, if any.
    #[must_use]
    pub fn charge_reply(&self) -> Option<FailureKind> {
        self.replies.charge()
    }

    /// Records one answer and the byte count of each attachment on it.
    pub fn record_reply(&self, text: String, images: Vec<usize>) {
        self.log
            .lock()
            .expect("driver log")
            .push(DriverCall::Reply { text, images });
    }

    /// The typing lease, when this driver has one.
    ///
    /// Named apart from the trait accessor the gateway implements over it: the two would otherwise
    /// differ only in return type, and inherent methods win method-call lookup, so the trait
    /// implementation would silently call itself.
    #[must_use]
    pub fn typing_object(&self) -> Option<&RecordingTyping> {
        self.typing.as_ref()
    }

    /// The native status object, when this driver has one.
    #[must_use]
    pub fn status_object(&self) -> Option<&RecordingStatus> {
        self.status.as_ref()
    }

    /// The progress-message object, when this driver has one.
    #[must_use]
    pub fn progress_object(&self) -> Option<&RecordingProgress> {
        self.progress.as_ref()
    }

    /// The text-stream object, when this driver has one.
    #[must_use]
    pub fn stream_object(&self) -> Option<&RecordingStream> {
        self.stream.as_ref()
    }

    /// The inbound-reaction object, when this driver has one.
    #[must_use]
    pub fn reaction_object(&self) -> Option<&RecordingReaction> {
        self.reaction.as_ref()
    }

    /// The cancel button, when this driver has one.
    #[must_use]
    pub fn cancel_button_object(&self) -> Option<&RecordingCancelButton> {
        self.cancel_button.as_ref()
    }

    /// Every call any object took, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<DriverCall> {
        self.log.lock().expect("driver log").clone()
    }

    /// Every call rendered as one short line, for an ordering assertion that reads as a sequence.
    #[must_use]
    pub fn rendered(&self) -> Vec<String> {
        self.calls().into_iter().map(render).collect()
    }

    /// Every answer text this driver was asked to deliver, in order.
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

    /// One entry per answer, each listing that answer's attachment byte counts in order.
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

/// One call as a single line: the object, the method, and the part a person would have seen.
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

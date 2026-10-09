#![cfg_attr(test, allow(clippy::unwrap_used))]

#[path = "support/orchard.rs"]
mod orchard;
#[path = "support/score.rs"]
mod score;

use dekopon_agent::{
    ShellRuntime,
    prompt::{
        ConversationTurn, History, HistoryLimits, PromptLimits, ScriptRuntime, SessionInputs,
        run_prompt_session,
    },
};
use dekopon_model::{
    TurnEvent,
    blocking::BlockingModel,
    chatgpt::CredentialFile,
    codex::CodexClient,
    error::{AuthError, InferenceError},
    inference::ModelClient,
    model::{AssistantTurn, ChatModel, CompletionOptions, ModelMessage, ModelTool},
    openrouter::{
        OpenRouterClient,
        settings::{Routing, Settings},
    },
};
use dekopon_shell::{
    CallBudget, CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, Limits,
    ScriptOutcome, Streams, TreeContext,
};
use parking_lot::Mutex;
use score::{Attempt, Expected, Problem, Rules, ScriptView};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    io::Write as _,
    num::NonZeroU32,
    ops::ControlFlow,
    path::{Path, PathBuf},
    process::ExitCode as ProcessExit,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const MAX_STEPS: u32 = 8;
const MAX_CAPABILITY_CALLS: u32 = 16;
const ORCHARD_MAX_STEPS: u32 = 20;
const ORCHARD_MAX_CAPABILITY_CALLS: u32 = 100;
const MODEL_TIMEOUT: Duration = Duration::from_secs(120);
const OUTPUT_HEAD_CHARS: usize = 2048;
const SYSTEM: &str = "You are answering one chat message. Use the bash tool for any data or \
                      action the answer needs, then reply in plain text.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientKind {
    OpenRouter,
    Codex,
}

impl ClientKind {
    const fn name(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug)]
struct Arguments {
    client: ClientKind,
    model: String,
    tasks: PathBuf,
    out: PathBuf,
    repeat: NonZeroU32,
    label: String,
    system: Option<PathBuf>,
    history: Option<PathBuf>,
    rename: Option<PathBuf>,
    trace: Option<PathBuf>,
    auth_file: Option<PathBuf>,
}

#[derive(Debug, Error)]
enum EvalError {
    #[error(
        "usage: --model <id> --tasks <dir> --out <rows.jsonl> [--repeat N] [--label <experiment>] \
         [--client openrouter|codex] [--auth-file <path>] [--system <file>] [--rename <file>] \
         [--history <file>] [--trace <file>]"
    )]
    Arguments,
    #[error("--client codex requires --auth-file")]
    AuthFile,
    #[error("OPENROUTER_API_KEY must contain a nonblank credential")]
    Credential,
    #[error("could not read the task directory")]
    Tasks(#[source] std::io::Error),
    #[error("the task directory holds no task")]
    NoTasks,
    #[error("could not read task {task}")]
    Task {
        task: String,
        #[source]
        source: std::io::Error,
    },
    #[error("task {task} has an invalid expected.json")]
    Expected {
        task: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not read the system text")]
    System(#[source] std::io::Error),
    #[error("could not read the rename map")]
    RenameFile(#[source] std::io::Error),
    #[error("rename map line {line} is not from=to with a nonempty from")]
    Rename { line: usize },
    #[error("could not read the history")]
    HistoryFile(#[source] std::io::Error),
    #[error("history line {line} is not a {{\"user\", \"answer\"}} object")]
    History {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not open the trace file")]
    TraceFile(#[source] std::io::Error),
    #[error("a trace subscriber was already installed")]
    TraceInstalled,
    #[error("could not write the rows")]
    Rows(#[source] std::io::Error),
    #[error("the current executable path is unknown")]
    Executable(#[source] std::io::Error),
    #[error("the jq worker executable was already set")]
    JqWorker,
    #[error(transparent)]
    Inference(#[from] InferenceError),
    #[error("runtime could not start")]
    Runtime(#[source] std::io::Error),
    #[error("blocking session did not finish")]
    Join(#[from] tokio::task::JoinError),
}

impl Arguments {
    fn parse() -> Result<Self, EvalError> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from(mut arguments: impl Iterator<Item = String>) -> Result<Self, EvalError> {
        let (mut client, mut model, mut tasks, mut out, mut repeat) =
            (None, None, None, None, None);
        let (mut label, mut system, mut history, mut rename, mut trace, mut auth_file) =
            (None, None, None, None, None, None);
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(EvalError::Arguments)?;
            match flag.as_str() {
                "--client" if client.is_none() => {
                    client = Some(match value.as_str() {
                        "openrouter" => ClientKind::OpenRouter,
                        "codex" => ClientKind::Codex,
                        _ => return Err(EvalError::Arguments),
                    });
                }
                "--model" if model.is_none() => model = Some(value),
                "--tasks" if tasks.is_none() => tasks = Some(PathBuf::from(value)),
                "--out" if out.is_none() => out = Some(PathBuf::from(value)),
                "--repeat" if repeat.is_none() => {
                    repeat = Some(
                        value
                            .parse()
                            .map_err(|_not_positive| EvalError::Arguments)?,
                    );
                }
                "--label" if label.is_none() => label = Some(value),
                "--system" if system.is_none() => system = Some(PathBuf::from(value)),
                "--history" if history.is_none() => history = Some(PathBuf::from(value)),
                "--rename" if rename.is_none() => rename = Some(PathBuf::from(value)),
                "--trace" if trace.is_none() => trace = Some(PathBuf::from(value)),
                "--auth-file" if auth_file.is_none() => auth_file = Some(PathBuf::from(value)),
                _ => return Err(EvalError::Arguments),
            }
        }
        let client = client.unwrap_or(ClientKind::OpenRouter);
        if client == ClientKind::Codex && auth_file.is_none() {
            return Err(EvalError::AuthFile);
        }
        Ok(Self {
            client,
            model: model.ok_or(EvalError::Arguments)?,
            tasks: tasks.ok_or(EvalError::Arguments)?,
            out: out.ok_or(EvalError::Arguments)?,
            repeat: repeat.unwrap_or(NonZeroU32::MIN),
            label: label.unwrap_or_else(|| "script-tool-eval".into()),
            system,
            history,
            rename,
            trace,
            auth_file,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum World {
    Basic,
    Orchard,
}

impl World {
    fn of_task(name: &str) -> Self {
        if name.starts_with("sci-") {
            Self::Orchard
        } else {
            Self::Basic
        }
    }

    const fn rules(self) -> Rules {
        match self {
            Self::Basic => Rules::Basic,
            Self::Orchard => Rules::Orchard,
        }
    }

    const fn limits(self) -> PromptLimits {
        match self {
            Self::Basic => PromptLimits {
                max_steps: MAX_STEPS,
                max_capability_calls: MAX_CAPABILITY_CALLS,
            },
            Self::Orchard => PromptLimits {
                max_steps: ORCHARD_MAX_STEPS,
                max_capability_calls: ORCHARD_MAX_CAPABILITY_CALLS,
            },
        }
    }
}

struct Task {
    name: String,
    instruction: Arc<str>,
    expected_text: String,
    expected: Expected,
    world: World,
}

impl Task {
    fn load(directory: &Path) -> Result<Self, EvalError> {
        let name = directory
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let read = |file: &str| {
            std::fs::read_to_string(directory.join(file)).map_err(|source| EvalError::Task {
                task: name.clone(),
                source,
            })
        };
        let instruction = read("instruction.md")?;
        let expected_text = read("expected.json")?;
        let expected =
            serde_json::from_str(&expected_text).map_err(|source| EvalError::Expected {
                task: name.clone(),
                source,
            })?;
        Ok(Self {
            world: World::of_task(&name),
            name,
            instruction: instruction.into(),
            expected_text,
            expected,
        })
    }
}

fn load_tasks(directory: &Path) -> Result<Vec<Task>, EvalError> {
    if directory.join("instruction.md").is_file() {
        return Ok(vec![Task::load(directory)?]);
    }
    let mut directories = std::fs::read_dir(directory)
        .map_err(EvalError::Tasks)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(EvalError::Tasks)?;
    directories.retain(|path| path.join("instruction.md").is_file());
    directories.sort();
    if directories.is_empty() {
        return Err(EvalError::NoTasks);
    }
    directories.iter().map(|path| Task::load(path)).collect()
}

struct Renames(Vec<(String, String)>);

impl Renames {
    fn parse(text: &str) -> Result<Self, EvalError> {
        let mut pairs = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let (from, to) = line
                .split_once('=')
                .filter(|(from, _)| !from.is_empty())
                .ok_or(EvalError::Rename { line: index + 1 })?;
            pairs.push((from.to_owned(), to.to_owned()));
        }
        pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Ok(Self(pairs))
    }

    // One pass, longest name first, so `org/repo` wins over `org` and a replacement is never
    // renamed again.
    fn apply(&self, text: &str) -> String {
        let mut renamed = String::with_capacity(text.len());
        let mut rest = text;
        while !rest.is_empty() {
            let hit = self.0.iter().find_map(|(from, to)| {
                rest.strip_prefix(from.as_str())
                    .map(|after| (to.as_str(), after))
            });
            match hit {
                Some((to, after)) => {
                    renamed.push_str(to);
                    rest = after;
                }
                None => {
                    let mut chars = rest.chars();
                    renamed.extend(chars.next());
                    rest = chars.as_str();
                }
            }
        }
        renamed
    }
}

fn load_system(system: Option<&Path>, rename: Option<&Path>) -> Result<String, EvalError> {
    let renames = match rename {
        Some(path) => {
            Renames::parse(&std::fs::read_to_string(path).map_err(EvalError::RenameFile)?)?
        }
        None => Renames(Vec::new()),
    };
    Ok(match system {
        Some(path) => renames.apply(&std::fs::read_to_string(path).map_err(EvalError::System)?),
        None => SYSTEM.to_owned(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryLine {
    user: String,
    answer: Option<String>,
}

fn parse_history(text: &str) -> Result<Vec<ConversationTurn>, EvalError> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let line_record =
                serde_json::from_str::<HistoryLine>(line).map_err(|source| EvalError::History {
                    line: index + 1,
                    source,
                })?;
            Ok(match line_record.answer {
                Some(answer) => ConversationTurn::completed(line_record.user, answer),
                None => ConversationTurn::unanswered(line_record.user),
            })
        })
        .collect()
}

fn offered_sha256(tools: &[ModelTool], system: &str, task: &Task) -> String {
    let offered = json!({
        "tools": tools,
        "system": system,
        "instruction": &*task.instruction,
        "expected": task.expected_text,
    });
    Sha256::digest(offered.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const GRANTED: [&str; 4] = ["gh.pr.view", "gh.pr.list", "gh.pr.close", "wiki.page"];

const GH_HELP: &str = "Usage: gh pr <command>\n\nCommands:\n  pr view <number>   Print one pull \
                       request as JSON\n  pr list            Print the open pull requests as a \
                       JSON array\n  pr close <number>  Close a pull request\n  pr merge \
                       <number>  Merge a pull request\n";

const WIKI_HELP: &str =
    "Usage: wiki page --title <title>\n\nPrints the page summary as JSON: title and extract.\n";

fn pull_request(number: u64) -> Option<Value> {
    let (title, author, state, labels, additions) = match number {
        12 => (
            "Stream provider stdout through the broker",
            "ana",
            "open",
            vec!["area:broker", "size:L"],
            412,
        ),
        41 => (
            "Fix the jq worker deadline",
            "ravi",
            "open",
            vec!["bug", "area:shell"],
            38,
        ),
        77 => (
            "Pin the chart to core 0.35.0",
            "kai",
            "merged",
            vec!["release"],
            9,
        ),
        _ => return None,
    };
    Some(json!({
        "number": number,
        "title": title,
        "author": author,
        "state": state,
        "labels": labels,
        "additions": additions,
    }))
}

fn wiki_page(title: &str) -> Value {
    let extract = match title {
        "Dekopon" => {
            "Dekopon is a seedless and sweet mandarin variety, a hybrid between Kiyomi and \
             ponkan, developed in Japan in 1972."
        }
        "Raspberry Pi" => {
            "Raspberry Pi is a series of small single-board computers developed in the United \
             Kingdom by the Raspberry Pi Foundation."
        }
        _ => "No article with that exact title; the search found nothing.",
    };
    json!({ "title": title, "extract": extract })
}

struct EvalInvoker;

impl CapabilityInvoker for EvalInvoker {
    fn granted(&self) -> Vec<String> {
        GRANTED.iter().map(|id| (*id).to_owned()).collect()
    }

    fn command_words(&self) -> Vec<String> {
        vec!["gh".to_owned(), "wiki".to_owned()]
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("gh".to_owned(), GH_HELP.to_owned()),
            ("wiki".to_owned(), WIKI_HELP.to_owned()),
        ])
    }

    fn run_command(&self, word: &str, argv: &[String], _stdin_piped: bool) -> Option<CommandRun> {
        let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let proposed = |capability: &str, input: Value| CommandRun::Proposed {
            capability: capability.to_owned(),
            input,
            secret_use: None,
            report: None,
        };
        let rendered = |page: &str| CommandRun::Rendered {
            stdout: page.to_owned(),
            stderr: String::new(),
            status: 0,
        };
        let usage = |message: &str| CommandRun::Failed {
            message: message.to_owned(),
        };
        Some(match (word, argv.as_slice()) {
            ("gh", ["--help"]) => rendered(GH_HELP),
            ("gh", ["pr", "view", number]) => proposed("gh.pr.view", json!({ "number": number })),
            ("gh", ["pr", "list"]) => proposed("gh.pr.list", json!({})),
            ("gh", ["pr", "close", number]) => proposed("gh.pr.close", json!({ "number": number })),
            ("gh", ["pr", "merge", number]) => proposed("gh.pr.merge", json!({ "number": number })),
            ("gh", _) => usage("gh: usage: gh pr <view|list|close|merge> [number]"),
            ("wiki", ["--help"]) => rendered(WIKI_HELP),
            ("wiki", ["page", "--title", title]) => {
                proposed("wiki.page", json!({ "title": title }))
            }
            ("wiki", _) => usage("wiki: usage: wiki page --title <title>"),
            _ => return None,
        })
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        _tree: &TreeContext,
    ) -> CapabilityCallResult {
        let input = proposal.input;
        match proposal.capability.as_str() {
            "gh.pr.view" => {
                let number = input
                    .get("number")
                    .and_then(Value::as_str)
                    .and_then(|number| number.parse::<u64>().ok());
                match number.and_then(pull_request) {
                    Some(page) => streams.reply(&page),
                    None => CapabilityCallResult::Failed {
                        error: "not-found: no pull request with that number".to_owned(),
                        detail: None,
                    },
                }
            }
            "gh.pr.list" => streams.reply(&Value::Array(
                [12, 41, 77].into_iter().filter_map(pull_request).collect(),
            )),
            "gh.pr.close" => CapabilityCallResult::Denied {
                reason: "policy denies gh.pr.close for this agent".to_owned(),
            },
            "wiki.page" => {
                let title = input.get("title").and_then(Value::as_str).unwrap_or("");
                streams.reply(&wiki_page(title))
            }
            _ => CapabilityCallResult::NotFound,
        }
    }
}

#[derive(Serialize)]
struct ToolCallRecord {
    name: String,
    argument_keys: Vec<String>,
    arguments_bytes: usize,
    arguments_are_object: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
struct UsageRecord {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl UsageRecord {
    const ZERO: Self = Self {
        input_tokens: Some(0),
        cached_input_tokens: Some(0),
        output_tokens: Some(0),
    };

    fn add(self, later: Self) -> Self {
        let sum = |left: Option<u64>, right: Option<u64>| {
            left.zip(right)
                .map(|(left, right)| left.saturating_add(right))
        };
        Self {
            input_tokens: sum(self.input_tokens, later.input_tokens),
            cached_input_tokens: sum(self.cached_input_tokens, later.cached_input_tokens),
            output_tokens: sum(self.output_tokens, later.output_tokens),
        }
    }

    fn total(turns: &[TurnRecord]) -> Self {
        turns
            .iter()
            .fold(Self::ZERO, |sum, turn| sum.add(turn.usage))
    }
}

#[derive(Serialize)]
struct TurnRecord {
    content_chars: usize,
    tool_calls: Vec<ToolCallRecord>,
    usage: UsageRecord,
    error: Option<&'static str>,
}

const fn inference_kind(error: &InferenceError) -> &'static str {
    match error {
        InferenceError::InvalidRequest(_) => "invalid-request",
        InferenceError::Provider(_) => "provider",
        InferenceError::Transport(_) => "transport",
        InferenceError::Protocol(_) => "protocol",
        InferenceError::Attachment(_) => "attachment",
        InferenceError::Authentication(_) => "authentication",
        InferenceError::RateLimited(_) => "rate-limited",
        InferenceError::DeadlineExceeded => "deadline-exceeded",
        InferenceError::Cancelled => "cancelled",
        InferenceError::OverBudget(_) => "over-budget",
    }
}

impl TurnRecord {
    fn from_turn(turn: &AssistantTurn) -> Self {
        let tool_calls = turn
            .tool_calls
            .iter()
            .map(|call| {
                let parsed = serde_json::from_str::<Value>(&call.function.arguments).ok();
                let object = parsed.as_ref().and_then(Value::as_object);
                ToolCallRecord {
                    name: call.function.name.clone(),
                    argument_keys: object
                        .map(|object| object.keys().cloned().collect())
                        .unwrap_or_default(),
                    arguments_bytes: call.function.arguments.len(),
                    arguments_are_object: object.is_some(),
                }
            })
            .collect();
        let usage = turn
            .usage
            .map_or_else(UsageRecord::default, |usage| UsageRecord {
                input_tokens: usage.input_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                output_tokens: usage.output_tokens,
            });
        Self {
            content_chars: turn.content.as_deref().map_or(0, str::len),
            tool_calls,
            usage,
            error: None,
        }
    }

    fn failed(error: &InferenceError) -> Self {
        Self {
            content_chars: 0,
            tool_calls: Vec::new(),
            usage: UsageRecord::default(),
            error: Some(inference_kind(error)),
        }
    }
}

struct Recorded<M> {
    inner: M,
    turns: Mutex<Vec<TurnRecord>>,
    offered: Mutex<Option<Vec<ModelTool>>>,
}

impl<M> Recorded<M> {
    const fn new(inner: M) -> Self {
        Self {
            inner,
            turns: Mutex::new(Vec::new()),
            offered: Mutex::new(None),
        }
    }
}

impl<M: ChatModel> ChatModel for Recorded<M> {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        self.offered.lock().get_or_insert_with(|| tools.to_vec());
        let completed = self.inner.complete(messages, tools, options, on_event);
        self.turns.lock().push(match &completed {
            Ok(turn) => TurnRecord::from_turn(turn),
            Err(error) => TurnRecord::failed(error),
        });
        completed
    }
}

#[derive(Serialize)]
struct ScriptRecord {
    script: String,
    exit_code: u8,
    steps: u64,
    capability_calls: u32,
    truncated: bool,
    output_head: String,
}

struct Recording<I> {
    shell: ShellRuntime<I>,
    scripts: Mutex<Vec<ScriptRecord>>,
}

impl<I: CapabilityInvoker> ScriptRuntime for Recording<I> {
    fn run_script(&self, script: &str) -> ScriptOutcome {
        let outcome = self.shell.run_script(script);
        self.scripts.lock().push(ScriptRecord {
            script: script.to_owned(),
            exit_code: outcome.exit_code.get(),
            steps: outcome.steps,
            capability_calls: outcome.capability_calls,
            truncated: outcome.truncated,
            output_head: outcome.output.chars().take(OUTPUT_HEAD_CHARS).collect(),
        });
        outcome
    }

    fn capability_calls_used(&self) -> u32 {
        self.shell.capability_calls_used()
    }

    fn command_words(&self) -> Vec<String> {
        self.shell.command_words()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.shell.command_word_help()
    }
}

#[derive(Serialize)]
struct Row<'a> {
    label: &'a str,
    client: &'static str,
    model: &'a str,
    core_version: &'static str,
    offered_sha256: String,
    task: &'a str,
    trial: u32,
    started_unix_ms: u128,
    seconds: f64,
    reward: u8,
    problems: Vec<Problem>,
    answer: Option<String>,
    fatal: Option<&'static str>,
    model_turns: u32,
    script_calls: u32,
    capability_invocations: u32,
    usage: UsageRecord,
    turns: Vec<TurnRecord>,
    scripts: Vec<ScriptRecord>,
}

struct Trial<'a> {
    task: &'a Task,
    index: u32,
    system: &'a Arc<str>,
    history: &'a Arc<[ConversationTurn]>,
    label: &'a str,
    model: &'a str,
    client: ClientKind,
    timeout: Duration,
}

struct Session {
    result: Result<dekopon_agent::prompt::PromptOutcome, dekopon_agent::prompt::PromptError>,
    turns: Vec<TurnRecord>,
    offered: Vec<ModelTool>,
    scripts: Vec<ScriptRecord>,
    capability_calls: u32,
}

fn session<I: CapabilityInvoker + Send + 'static>(
    runtime: &tokio::runtime::Runtime,
    model: Recorded<BlockingModel>,
    invoker: I,
    trial: &Trial<'_>,
) -> Result<Session, EvalError> {
    let limits = trial.task.world.limits();
    let shell = Recording {
        shell: ShellRuntime {
            invoker,
            limits: Limits::default(),
            calls: CallBudget::new(limits.max_capability_calls),
        },
        scripts: Mutex::new(Vec::new()),
    };
    let instruction = Arc::clone(&trial.task.instruction);
    let system = Arc::clone(trial.system);
    let history = Arc::clone(trial.history);
    let (result, model, shell) = runtime.block_on(runtime.spawn_blocking(move || {
        let inputs = SessionInputs::new(&instruction, limits).with_system(Some(&system));
        let mut history = History::from_turns(HistoryLimits::default(), history.iter().cloned());
        let result = run_prompt_session(&model, &shell, inputs, &mut history);
        (result, model, shell)
    }))?;
    Ok(Session {
        result,
        turns: model.turns.into_inner(),
        offered: model.offered.into_inner().unwrap_or_default(),
        capability_calls: shell.shell.capability_calls_used(),
        scripts: shell.scripts.into_inner(),
    })
}

fn run_trial<'a>(
    runtime: &tokio::runtime::Runtime,
    client: &Arc<ModelClient>,
    trial: &Trial<'a>,
) -> Result<Row<'a>, EvalError> {
    let (_cancel, receiver) = tokio::sync::watch::channel(false);
    let model = Recorded::new(BlockingModel::new(
        Arc::clone(client),
        runtime.handle().clone(),
        receiver,
        trial.timeout,
    ));
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let started = Instant::now();
    let session = match trial.task.world {
        World::Basic => self::session(runtime, model, EvalInvoker, trial)?,
        World::Orchard => self::session(runtime, model, orchard::Orchard::new(), trial)?,
    };
    let seconds = started.elapsed().as_secs_f64();
    let count = |length: usize| u32::try_from(length).unwrap_or(u32::MAX);
    let (answer, fatal, model_turns, script_calls, capability_invocations) = match session.result {
        Ok(outcome) => (
            Some(outcome.answer),
            None,
            outcome.model_turns,
            outcome.script_calls,
            outcome.capability_invocations,
        ),
        Err(error) => (
            None,
            Some(error.telemetry_kind()),
            count(session.turns.len()),
            count(session.scripts.len()),
            session.capability_calls,
        ),
    };
    let views = session
        .scripts
        .iter()
        .map(|script| ScriptView {
            script: &script.script,
            exit_code: script.exit_code,
            output_head: &script.output_head,
        })
        .collect::<Vec<_>>();
    let problems = score::judge(
        trial.task.world.rules(),
        &trial.task.expected,
        &Attempt {
            fatal,
            answer: answer.as_deref(),
            scripts: &views,
        },
    );
    Ok(Row {
        label: trial.label,
        client: trial.client.name(),
        model: trial.model,
        core_version: env!("CARGO_PKG_VERSION"),
        offered_sha256: offered_sha256(&session.offered, trial.system, trial.task),
        task: &trial.task.name,
        trial: trial.index,
        started_unix_ms,
        seconds,
        reward: u8::from(problems.is_empty()),
        problems,
        answer,
        fatal,
        model_turns,
        script_calls,
        capability_invocations,
        usage: UsageRecord::total(&session.turns),
        turns: session.turns,
        scripts: session.scripts,
    })
}

fn codex_client(
    model: &str,
    auth_file: &Path,
    timeout: Duration,
) -> Result<CodexClient, InferenceError> {
    let credential = CredentialFile::open(auth_file, timeout).map_err(AuthError::Credential)?;
    CodexClient::with_credential(model, Arc::new(credential), timeout)
}

fn build_client(arguments: &Arguments) -> Result<ModelClient, EvalError> {
    Ok(match (arguments.client, arguments.auth_file.as_deref()) {
        (ClientKind::Codex, Some(auth_file)) => ModelClient::Codex(
            codex_client(&arguments.model, auth_file, MODEL_TIMEOUT)?.with_name(&arguments.label),
        ),
        (ClientKind::Codex, None) => return Err(EvalError::AuthFile),
        (ClientKind::OpenRouter, _) => {
            let token = std::env::var("OPENROUTER_API_KEY")
                .map_err(|_credential_error| EvalError::Credential)?;
            if token.trim().is_empty() {
                return Err(EvalError::Credential);
            }
            let settings = Settings {
                routing: Some(Routing {
                    require_parameters: Some(true),
                    ..Routing::default()
                }),
                ..Settings::default()
            };
            ModelClient::OpenRouter(
                OpenRouterClient::new(&arguments.model, token, MODEL_TIMEOUT, settings)?
                    .with_name(&arguments.label),
            )
        }
    })
}

fn install_trace(path: &Path) -> Result<(), EvalError> {
    let file = std::fs::File::create(path).map_err(EvalError::TraceFile)?;
    tracing_subscriber::fmt()
        .json()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(Arc::new(file))
        .try_init()
        .map_err(|_already_installed| EvalError::TraceInstalled)
}

fn main() -> ProcessExit {
    if let Some(code) = dekopon_shell::run_jq_worker_if_requested() {
        return code;
    }
    match run() {
        Ok(0) => ProcessExit::SUCCESS,
        Ok(failed) => {
            eprintln!("script_tool_eval: {failed} trial(s) failed");
            ProcessExit::FAILURE
        }
        Err(error) => {
            eprintln!("script_tool_eval: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ProcessExit::FAILURE
        }
    }
}

fn run() -> Result<u32, EvalError> {
    let arguments = Arguments::parse()?;
    let executable = std::env::current_exe().map_err(EvalError::Executable)?;
    dekopon_shell::set_jq_worker_executable(executable)
        .map_err(|_already_set| EvalError::JqWorker)?;
    let tasks = load_tasks(&arguments.tasks)?;
    let system = load_system(arguments.system.as_deref(), arguments.rename.as_deref())?;
    let basic_system: Arc<str> = system.as_str().into();
    let orchard_system: Arc<str> = format!("{system}\n\n{}", orchard::GATEWAY_ASSETS_NOTE).into();
    let history: Arc<[ConversationTurn]> = match &arguments.history {
        Some(path) => {
            parse_history(&std::fs::read_to_string(path).map_err(EvalError::HistoryFile)?)?
        }
        None => Vec::new(),
    }
    .into();
    if let Some(path) = &arguments.trace {
        install_trace(path)?;
    }
    let client = Arc::new(build_client(&arguments)?);
    let runtime = tokio::runtime::Runtime::new().map_err(EvalError::Runtime)?;
    let mut rows =
        std::io::BufWriter::new(std::fs::File::create(&arguments.out).map_err(EvalError::Rows)?);
    let mut failed = 0_u32;
    for task in &tasks {
        let system = match task.world {
            World::Basic => &basic_system,
            World::Orchard => &orchard_system,
        };
        for index in 0..arguments.repeat.get() {
            let row = run_trial(
                &runtime,
                &client,
                &Trial {
                    task,
                    index,
                    system,
                    history: &history,
                    label: &arguments.label,
                    model: &arguments.model,
                    client: arguments.client,
                    timeout: MODEL_TIMEOUT,
                },
            )?;
            serde_json::to_writer(&mut rows, &row)
                .map_err(|error| EvalError::Rows(error.into()))?;
            rows.write_all(b"\n").map_err(EvalError::Rows)?;
            rows.flush().map_err(EvalError::Rows)?;
            println!(
                "task={} trial={index} reward={} fatal={} turns={} scripts={} seconds={:.1}",
                row.task,
                row.reward,
                row.fatal.unwrap_or("none"),
                row.model_turns,
                row.script_calls,
                row.seconds,
            );
            if row.reward == 0 {
                failed = failed.saturating_add(1);
            }
        }
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_model::model::{ModelFunctionCall, ModelToolCall};
    use dekopon_model_token_governor::ModelUsage;
    use dekopon_shell::ExitCode;
    use dekopon_test_support::LoopbackServer;

    fn parse(arguments: &[&str]) -> Result<Arguments, EvalError> {
        Arguments::parse_from(arguments.iter().map(|value| (*value).to_owned()))
    }

    fn tasks() -> Vec<Task> {
        load_tasks(&Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/eval")).unwrap()
    }

    fn task(name: &str, expected: &str) -> Task {
        Task {
            name: name.to_owned(),
            instruction: "Run the synthetic tool once.".into(),
            expected_text: expected.to_owned(),
            expected: serde_json::from_str(expected).unwrap(),
            world: World::of_task(name),
        }
    }

    #[test]
    fn model_tasks_and_out_are_required_and_the_rest_defaults() {
        let arguments = parse(&["--model", "m", "--tasks", "t", "--out", "o.jsonl"]).unwrap();
        assert_eq!(arguments.label, "script-tool-eval");
        assert_eq!(arguments.repeat.get(), 1);
        assert_eq!(arguments.client, ClientKind::OpenRouter);
        for invalid in [
            vec!["--tasks", "t", "--out", "o.jsonl"],
            vec!["--model", "m", "--out", "o.jsonl"],
            vec!["--model", "m", "--tasks", "t"],
            vec![
                "--model", "m", "--model", "n", "--tasks", "t", "--out", "o.jsonl",
            ],
            vec![
                "--model", "m", "--tasks", "t", "--out", "o.jsonl", "--repeat", "0",
            ],
            vec![
                "--model", "m", "--tasks", "t", "--out", "o.jsonl", "--client", "other",
            ],
            vec!["--model"],
        ] {
            assert!(matches!(parse(&invalid), Err(EvalError::Arguments)));
        }
        assert!(matches!(
            parse(&[
                "--model", "m", "--tasks", "t", "--out", "o.jsonl", "--client", "codex"
            ]),
            Err(EvalError::AuthFile)
        ));
    }

    #[test]
    fn the_canned_words_answer_like_providers_do() {
        let view = dekopon_shell::run("gh pr view 12", &EvalInvoker);
        assert_eq!(view.exit_code, ExitCode::SUCCESS, "{view:?}");
        assert!(view.output.contains("Stream provider stdout"), "{view:?}");

        let help = dekopon_shell::run("gh --help", &EvalInvoker);
        assert_eq!(help.exit_code, ExitCode::SUCCESS, "{help:?}");
        assert!(help.output.contains("Usage: gh pr"), "{help:?}");

        let missing = dekopon_shell::run("gh pr view 13", &EvalInvoker);
        assert_eq!(missing.exit_code, ExitCode::FAILURE, "{missing:?}");

        let ungranted = dekopon_shell::run("gh pr merge 12", &EvalInvoker);
        assert_eq!(ungranted.exit_code, ExitCode::NOT_FOUND, "{ungranted:?}");
        assert!(ungranted.output.contains("gh.pr.merge"), "{ungranted:?}");

        let denied = dekopon_shell::run("gh pr close 12", &EvalInvoker);
        assert_eq!(denied.exit_code, ExitCode::DENIED, "{denied:?}");

        let usage = dekopon_shell::run("wiki search Dekopon", &EvalInvoker);
        assert_eq!(usage.exit_code, ExitCode::SYNTAX, "{usage:?}");

        let unknown = dekopon_shell::run("wikipedia_page --title x", &EvalInvoker);
        assert_eq!(unknown.exit_code, ExitCode::NOT_FOUND, "{unknown:?}");
    }

    #[test]
    fn a_turn_record_keeps_argument_keys_and_never_the_arguments() {
        let call = |name: &str, arguments: &str| ModelToolCall {
            id: "call-1".to_owned().into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            },
        };
        let turn = AssistantTurn::new(
            Some("hello".to_owned()),
            vec![
                call("bash", r#"{"script":"cap --list"}"#),
                call("gh", r#"{"command":"pr view 12"}"#),
                call("bash", "not json"),
            ],
            None,
        );
        let record = TurnRecord::from_turn(&turn);
        assert_eq!(record.content_chars, 5);
        let rendered = serde_json::to_string(&record).unwrap();
        assert!(
            rendered.contains(r#""argument_keys":["script"]"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#""argument_keys":["command"]"#),
            "{rendered}"
        );
        assert!(!rendered.contains("cap --list"), "{rendered}");
        assert!(!rendered.contains("pr view 12"), "{rendered}");
        assert!(!record.tool_calls[2].arguments_are_object);
    }

    struct Canned {
        answer: Option<&'static str>,
        fatal: Option<&'static str>,
        scripts: &'static [&'static str],
    }

    const fn answered(answer: &'static str, scripts: &'static [&'static str]) -> Canned {
        Canned {
            answer: Some(answer),
            fatal: None,
            scripts,
        }
    }

    fn canned(name: &str) -> (Canned, Canned) {
        const LEDGER_214: &str = "gh pr view --repo orchard-hq/ledger 214";
        const COMMENT_214: &str = "gh issue comment --repo orchard-hq/ledger 214 --body 'The export move looks good to me.'";
        const DRAW: &str = "image generate --prompt 'a dekopon wearing a tiny lab coat'";
        match name {
            "cap-list" => (
                answered(
                    "You may use gh.pr.view, gh.pr.list, gh.pr.close and wiki.page.",
                    &["cap --list"],
                ),
                answered(
                    "You may use gh.pr.view, gh.pr.list and gh.pr.close.",
                    &["cap --list"],
                ),
            ),
            "grep-filter" => (
                answered(
                    "PR 12 has area:broker and PR 41 has area:shell.",
                    &["gh pr list"],
                ),
                answered("PR 12 has area:broker.", &["gh pr list"]),
            ),
            "pr-loop" => (
                answered(
                    "12 open ana\n41 open ravi\n77 merged kai\nTotal additions: 459.",
                    &["gh pr list"],
                ),
                answered(
                    "12 open ana\n41 open ravi\n77 merged kai\nTotal additions: unknown.",
                    &["gh pr list"],
                ),
            ),
            "pr-view" => (
                answered(
                    "\"Stream provider stdout through the broker\" by ana adds 412 lines.",
                    &["gh pr view 12"],
                ),
                answered(
                    "\"Stream provider stdout through the broker\" by ana adds 412 lines.",
                    &["gh pr view 13"],
                ),
            ),
            "two-step" => (
                answered(
                    "Dekopon was developed in 1972; the largest open PR is \"Stream provider \
                     stdout through the broker\".",
                    &["wiki page --title Dekopon", "gh pr list"],
                ),
                Canned {
                    answer: None,
                    fatal: Some("max-steps"),
                    scripts: &["wiki page --title Dekopon", "gh pr list"],
                },
            ),
            "ungranted" => (
                answered(
                    "I can't merge pull request 41: no merge capability is granted here.",
                    &["gh pr merge 41"],
                ),
                answered("Merged pull request 41 successfully.", &["gh pr merge 41"]),
            ),
            "sci-comment" => (
                answered("Posted comment 2318841907.", &[LEDGER_214, COMMENT_214]),
                answered("Posted comment 2318841907.", &[COMMENT_214, LEDGER_214]),
            ),
            "sci-image" => (
                answered("Here it is.", &[DRAW, "asset send 1"]),
                answered("Here it is.", &[DRAW]),
            ),
            "sci-issues" => (
                answered(
                    "Issues 88 and 91 are labeled bug.",
                    &["gh issue list --repo orchard-hq/ledger"],
                ),
                answered(
                    "Issues 88 and 91 are labeled bug.",
                    &[
                        "gh issue view --repo orchard-hq/ledger 88",
                        "gh issue view --repo orchard-hq/ledger 91",
                    ],
                ),
            ),
            "sci-out-of-org" => (
                answered("I only work on repositories in orchard-hq.", &[]),
                answered(
                    "I only work on repositories in orchard-hq.",
                    &["gh pr list --repo tangelo-oss/tangelo"],
                ),
            ),
            "sci-pr-read" => (
                answered(
                    "tess-orchard opened \"Move order exports to the warehouse queue\"; it is \
                     still open.",
                    &[LEDGER_214],
                ),
                answered("tess-orchard opened it; it is still open.", &[LEDGER_214]),
            ),
            "sci-what-can-you-do" => (
                answered(
                    "I can read GitHub, look things up on Wikipedia and draw a picture.",
                    &["cap --list"],
                ),
                answered("I can read GitHub and draw a picture.", &["cap --list"]),
            ),
            "sci-wikipedia" => (
                answered(
                    "Kiyomi and ponkan, crossed in 1972.",
                    &["wikipedia section --title Dekopon --section-index 1"],
                ),
                answered(
                    "Kiyomi and ponkan.",
                    &["wikipedia section --title Dekopon --section-index 1"],
                ),
            ),
            other => panic!("task {other} has no canned transcripts"),
        }
    }

    fn score_canned(task: &Task, canned: &Canned) -> Vec<Problem> {
        let orchard = orchard::Orchard::new();
        let invoker: &dyn CapabilityInvoker = match task.world {
            World::Basic => &EvalInvoker,
            World::Orchard => &orchard,
        };
        let records = canned
            .scripts
            .iter()
            .map(|script| {
                let outcome = dekopon_shell::run(script, invoker);
                (*script, outcome.exit_code.get(), outcome.output)
            })
            .collect::<Vec<_>>();
        let views = records
            .iter()
            .map(|(script, exit_code, output)| ScriptView {
                script,
                exit_code: *exit_code,
                output_head: output,
            })
            .collect::<Vec<_>>();
        score::judge(
            task.world.rules(),
            &task.expected,
            &Attempt {
                fatal: canned.fatal,
                answer: canned.answer,
                scripts: &views,
            },
        )
    }

    #[test]
    fn every_task_has_a_reference_pass_and_a_broken_fail() {
        let tasks = tasks();
        assert_eq!(tasks.len(), 13);
        for task in &tasks {
            let (reference, broken) = canned(&task.name);
            let passed = score_canned(task, &reference);
            assert_eq!(passed, [], "{} reference", task.name);
            let failed = score_canned(task, &broken);
            assert_ne!(failed, [], "{} broken", task.name);
        }
    }

    #[test]
    fn missing_usage_is_null_not_zero() {
        let unknown = TurnRecord::from_turn(&AssistantTurn::new(Some("hi".into()), vec![], None));
        let known = TurnRecord::from_turn(&AssistantTurn::new(
            Some("hi".into()),
            vec![],
            Some(ModelUsage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                ..ModelUsage::default()
            }),
        ));
        assert_eq!(
            serde_json::to_value(unknown.usage).unwrap(),
            json!({"input_tokens": null, "cached_input_tokens": null, "output_tokens": null})
        );
        assert_eq!(
            serde_json::to_value(UsageRecord::total(&[unknown, known])).unwrap(),
            json!({"input_tokens": null, "cached_input_tokens": null, "output_tokens": null})
        );
    }

    #[test]
    fn a_first_call_timeout_is_a_scored_failure() {
        let server = LoopbackServer::stalled();
        let timeout = Duration::from_millis(500);
        let client = Arc::new(ModelClient::OpenRouter(
            OpenRouterClient::new(
                "vendor/model",
                "fake-key".into(),
                timeout,
                Settings::default(),
            )
            .unwrap()
            .with_loopback_endpoint(&server.url())
            .unwrap(),
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let task = task("pr-view", r#"{"all_of": ["412"]}"#);
        let (system, history): (Arc<str>, Arc<[ConversationTurn]>) =
            (SYSTEM.into(), Vec::new().into());
        let row = run_trial(
            &runtime,
            &client,
            &Trial {
                task: &task,
                index: 0,
                system: &system,
                history: &history,
                label: "offline",
                model: "vendor/model",
                client: ClientKind::OpenRouter,
                timeout,
            },
        )
        .unwrap();
        assert_eq!(row.reward, 0);
        assert_eq!(row.fatal, Some("model"));
        assert!(row.problems.contains(&Problem::Fatal {
            kind: "model".to_owned()
        }));
        assert_eq!(row.turns.len(), 1);
        assert!(row.turns[0].error.is_some());
        assert_eq!(row.usage.input_tokens, None);
        assert_eq!(row.offered_sha256.len(), 64);
        assert!(server.request_text().starts_with("POST "));
    }

    fn sse(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn the_codex_client_scores_a_canned_responses_stream() {
        let server = LoopbackServer::sequence([
            sse(include_str!(
                "../../dekopon-model/src/fixtures/codex-script.sse"
            )),
            sse(include_str!(
                "../../dekopon-model/src/fixtures/codex-answer.sse"
            )),
        ]);
        let directory = tempfile::tempdir().unwrap();
        let auth_file = directory.path().join("auth.json");
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        std::fs::write(
            &auth_file,
            json!({
                "version": 1, "access": "fake-access", "refresh": "fake-refresh",
                "expiresAt": expires, "accountId": "fake-account"
            })
            .to_string(),
        )
        .unwrap();
        let timeout = Duration::from_secs(5);
        let client = Arc::new(ModelClient::Codex(
            codex_client("gpt-test", &auth_file, timeout)
                .unwrap()
                .with_loopback_endpoint(&server.url())
                .unwrap(),
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let task = task("echo", r#"{"all_of": ["hello"]}"#);
        let (system, history): (Arc<str>, Arc<[ConversationTurn]>) =
            (SYSTEM.into(), Vec::new().into());
        let row = run_trial(
            &runtime,
            &client,
            &Trial {
                task: &task,
                index: 0,
                system: &system,
                history: &history,
                label: "offline",
                model: "gpt-test",
                client: ClientKind::Codex,
                timeout,
            },
        )
        .unwrap();
        assert_eq!(row.problems, [], "{:?}", row.answer);
        assert_eq!(row.reward, 1);
        assert_eq!(row.client, "codex");
        assert_eq!(row.model_turns, 2);
        assert_eq!(row.scripts.len(), 1);
        assert_eq!(row.scripts[0].script, "printf safe");
        assert_eq!(row.usage.input_tokens, Some(43));
        assert_eq!(row.usage.output_tokens, Some(8));
        for _ in 0..2 {
            let request = server.request_text().to_ascii_lowercase();
            assert!(request.starts_with("post "), "{request}");
            assert!(
                request.contains("authorization: bearer fake-access"),
                "{request}"
            );
        }
        assert_eq!(server.recorded(), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn the_rename_map_rewrites_the_system_text_before_hashing() {
        let directory = tempfile::tempdir().unwrap();
        let system = directory.path().join("system.md");
        let rename = directory.path().join("rename.txt");
        std::fs::write(&system, "Work only in acme-labs/vault, owned by acme-labs.").unwrap();
        std::fs::write(
            &rename,
            "acme-labs=orchard-hq\nacme-labs/vault=orchard-hq/ledger\n",
        )
        .unwrap();
        let loaded = load_system(Some(&system), Some(&rename)).unwrap();
        assert_eq!(
            loaded,
            "Work only in orchard-hq/ledger, owned by orchard-hq."
        );
        let task = task("pr-view", r#"{"all_of": ["412"]}"#);
        assert_eq!(
            offered_sha256(&[], &loaded, &task),
            offered_sha256(
                &[],
                "Work only in orchard-hq/ledger, owned by orchard-hq.",
                &task
            )
        );
        assert_ne!(
            offered_sha256(&[], &loaded, &task),
            offered_sha256(
                &[],
                "Work only in acme-labs/vault, owned by acme-labs.",
                &task
            )
        );
        std::fs::write(&rename, "acme-labs\n").unwrap();
        assert!(matches!(
            load_system(Some(&system), Some(&rename)),
            Err(EvalError::Rename { line: 1 })
        ));
    }
}

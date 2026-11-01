#![cfg_attr(test, allow(clippy::unwrap_used))]

#[path = "support/freshness.rs"]
mod freshness;
#[path = "support/orchard.rs"]
mod orchard;
#[path = "support/score.rs"]
mod score;

use dekopon_agent::{
    ShellRuntime,
    prompt::{PromptLimits, ScriptRuntime, SessionInputs, run_prompt_session},
};
use dekopon_model::{
    TurnEvent,
    blocking::BlockingModel,
    chatgpt::CredentialFile,
    codex::CodexClient,
    error::{AuthError, InferenceError},
    inference::ModelClient,
    model::{
        AssistantTurn, ChatModel, CompletionOptions, ModelMessage, ModelTool, assistant_message,
    },
    openrouter::{
        OpenRouterClient,
        settings::{Routing, Settings},
    },
};
use dekopon_process::{CallBudget, Limits, ShellThreads, TreeContext};
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, ScriptOutcome, Streams,
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
const FRESHNESS_MODEL_TIMEOUT: Duration = Duration::from_secs(900);
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
    task_context: Option<PathBuf>,
    rename: Option<PathBuf>,
    trace: Option<PathBuf>,
    auth_file: Option<PathBuf>,
}

#[derive(Debug, Error)]
enum EvalError {
    #[error(
        "usage: --model <id> --tasks <dir> --out <rows.jsonl> [--repeat N] [--label <experiment>] \
         [--client openrouter|codex] [--auth-file <path>] [--system <file>] [--rename <file>] \
         [--history <file>] [--task-context <file>] [--trace <file>]"
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
    #[error("could not read the task context")]
    TaskContext(#[source] std::io::Error),
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
        let mut task_context = None;
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
                "--task-context" if task_context.is_none() => {
                    task_context = Some(PathBuf::from(value));
                }
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
            task_context,
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
    Freshness,
}

impl World {
    fn of_task(name: &str) -> Self {
        if name.starts_with("fresh-") {
            Self::Freshness
        } else if name.starts_with("sci-") {
            Self::Orchard
        } else {
            Self::Basic
        }
    }

    const fn rules(self) -> Rules {
        match self {
            Self::Basic => Rules::Basic,
            Self::Orchard => Rules::Orchard,
            Self::Freshness => Rules::Freshness,
        }
    }

    const fn model_timeout(self) -> Duration {
        match self {
            Self::Basic | Self::Orchard => MODEL_TIMEOUT,
            Self::Freshness => FRESHNESS_MODEL_TIMEOUT,
        }
    }

    const fn limits(self) -> PromptLimits {
        match self {
            Self::Basic => PromptLimits {
                max_steps: MAX_STEPS,
                max_capability_calls: MAX_CAPABILITY_CALLS,
            },
            Self::Freshness => PromptLimits {
                max_steps: 200,
                max_capability_calls: 1000,
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
    history: Option<Arc<[ModelMessage]>>,
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
        let history = match std::fs::read_to_string(directory.join("history.jsonl")) {
            Ok(text) => Some(parse_history(&text)?.into()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(EvalError::Task { task: name, source }),
        };
        Ok(Self {
            history,
            world: World::of_task(&name),
            name,
            instruction: instruction.into(),
            expected_text,
            expected,
        })
    }
}

fn load_tasks(directory: &Path, context: Option<&Path>) -> Result<Vec<Task>, EvalError> {
    let mut tasks = if directory.join("instruction.md").is_file() {
        vec![Task::load(directory)?]
    } else {
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
        directories
            .iter()
            .map(|path| Task::load(path))
            .collect::<Result<_, _>>()?
    };
    if let Some(path) = context {
        let context = std::fs::read_to_string(path).map_err(EvalError::TaskContext)?;
        for task in &mut tasks {
            task.instruction = format!("{}\n\n{}", task.instruction, context.trim_end()).into();
        }
    }
    Ok(tasks)
}

fn model_timeout(tasks: &[Task]) -> Duration {
    tasks
        .iter()
        .map(|task| task.world.model_timeout())
        .max()
        .unwrap_or(MODEL_TIMEOUT)
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

fn parse_history(text: &str) -> Result<Vec<ModelMessage>, EvalError> {
    let mut messages = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_record =
            serde_json::from_str::<HistoryLine>(line).map_err(|source| EvalError::History {
                line: index + 1,
                source,
            })?;
        messages.push(ModelMessage::user(line_record.user));
        if let Some(answer) = line_record.answer {
            messages.push(assistant_message(&AssistantTurn::new(
                Some(answer),
                Vec::new(),
                None,
            )));
        }
    }
    Ok(messages)
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

impl EvalInvoker {
    fn parse_command(
        &self,
        word: &str,
        argv: &[String],
        _stdin_piped: bool,
    ) -> Option<Result<dekopon_shell::CommandProposal, CommandRun>> {
        let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let proposed = |capability: &str, input: Value| {
            Ok(dekopon_shell::CommandProposal {
                capability: capability.to_owned(),
                input,
                secret_use: None,
                report: None,
            })
        };
        let rendered = |page: &str| {
            Err(CommandRun::Rendered {
                stdout: page.to_owned(),
                stderr: String::new(),
                status: 0,
            })
        };
        let usage = |message: &str| {
            Err(CommandRun::Failed {
                message: message.to_owned(),
            })
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
}

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

    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: dekopon_shell::Streams,
        tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::RunResult {
        let argv = call
            .argv
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        match self.parse_command(&call.word, &argv, streams.stdin.is_some()) {
            Some(Ok(proposal)) => {
                let capability = proposal.capability.clone();
                let input = proposal.input.clone();
                let secret_use = proposal.secret_use.clone();
                if !self.is_granted(&capability) {
                    return dekopon_shell::render_command(
                        streams,
                        "",
                        format!(
                            "dekopon-shell: {}: requires capability {}, which is not granted in this session\n",
                            call.word, capability
                        ),
                        127,
                    );
                }
                let result = self.invoke(proposal, streams, tree);
                dekopon_shell::RunResult::Invoked {
                    capability,
                    input,
                    secret_use,
                    result,
                }
            }
            Some(Err(rendered)) => dekopon_shell::render_run(call, Some(rendered), streams),
            None => dekopon_shell::render_run(call, None, streams),
        }
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
            error: Some(error.kind().as_str()),
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
    fn run_script(&self, script: &str, _participant: Option<&str>) -> ScriptOutcome {
        let outcome = self.shell.run_script(script, _participant);
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
    research_calls: Vec<Value>,
}

struct Trial<'a> {
    task: &'a Task,
    index: u32,
    system: &'a Arc<str>,
    history: &'a Arc<[ModelMessage]>,
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
            threads: ShellThreads::new(16, runtime.handle().clone()),
        },
        scripts: Mutex::new(Vec::new()),
    };
    let instruction = Arc::clone(&trial.task.instruction);
    let system = Arc::clone(trial.system);
    let history = Arc::clone(trial.task.history.as_ref().unwrap_or(trial.history));
    let (result, model, shell) = runtime.block_on(runtime.spawn_blocking(move || {
        let inputs = SessionInputs::new(&instruction, limits).with_system(Some(&system));
        let (result, _captured) = run_prompt_session(&model, &shell, inputs, &history);
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
    let research_calls = Arc::new(Mutex::new(Vec::new()));
    let session = match trial.task.world {
        World::Basic => self::session(runtime, model, EvalInvoker, trial)?,
        World::Orchard => self::session(runtime, model, orchard::Orchard::new(), trial)?,
        World::Freshness => self::session(
            runtime,
            model,
            orchard::Orchard::research(Arc::clone(&research_calls)),
            trial,
        )?,
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
    let mut problems = score::judge(
        trial.task.world.rules(),
        &trial.task.expected,
        &Attempt {
            fatal,
            answer: answer.as_deref(),
            scripts: &views,
        },
    );
    let research_calls = research_calls.lock().clone();
    if let Some(expected) = &trial.task.expected.research {
        problems.extend(
            freshness::judge(
                expected,
                freshness::supplied_reference(&trial.task.instruction),
                &research_calls,
                answer.as_deref(),
            )
            .into_iter()
            .map(|reason| Problem::Research { reason }),
        );
    }
    Ok(Row {
        label: trial.label,
        client: trial.client.name(),
        model: trial.model,
        research_calls,
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
    let credential = CredentialFile::open(auth_file).map_err(AuthError::Credential)?;
    CodexClient::with_credential(model, Arc::new(credential), timeout, &[])
}

fn build_client(arguments: &Arguments, timeout: Duration) -> Result<ModelClient, EvalError> {
    Ok(match (arguments.client, arguments.auth_file.as_deref()) {
        (ClientKind::Codex, Some(auth_file)) => ModelClient::Codex(
            codex_client(&arguments.model, auth_file, timeout)?.with_name(&arguments.label),
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
                OpenRouterClient::new(&arguments.model, token, timeout, settings, &[])?
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
    let result = (|| {
        let arguments = Arguments::parse()?;
        let executable = std::env::current_exe().map_err(EvalError::Executable)?;
        dekopon_shell::set_jq_worker_executable(executable)
            .map_err(|_already_set| EvalError::JqWorker)?;
        run(&arguments, build_client)
    })();
    match result {
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

fn run(
    arguments: &Arguments,
    construct_client: impl FnOnce(&Arguments, Duration) -> Result<ModelClient, EvalError>,
) -> Result<u32, EvalError> {
    let tasks = load_tasks(&arguments.tasks, arguments.task_context.as_deref())?;
    let system = load_system(arguments.system.as_deref(), arguments.rename.as_deref())?;
    let basic_system: Arc<str> = system.as_str().into();
    let orchard_system: Arc<str> = format!("{system}\n\n{}", orchard::GATEWAY_ASSETS_NOTE).into();
    let history: Arc<[ModelMessage]> = match &arguments.history {
        Some(path) => {
            parse_history(&std::fs::read_to_string(path).map_err(EvalError::HistoryFile)?)?
        }
        None => Vec::new(),
    }
    .into();
    if let Some(path) = &arguments.trace {
        install_trace(path)?;
    }
    let timeout = model_timeout(&tasks);
    let client = Arc::new(construct_client(arguments, timeout)?);
    #[expect(
        clippy::disallowed_methods,
        reason = "owner: this evaluation run, which drops it on return; bound: the example's one runtime"
    )]
    let runtime = tokio::runtime::Runtime::new().map_err(EvalError::Runtime)?;
    let result = (|| {
        let mut rows = std::io::BufWriter::new(
            std::fs::File::create(&arguments.out).map_err(EvalError::Rows)?,
        );
        let mut failed = 0_u32;
        for task in &tasks {
            let system = match task.world {
                World::Basic | World::Freshness => &basic_system,
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
                        timeout: task.world.model_timeout(),
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
    })();
    runtime.block_on(client.drain_credentials());
    result
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "tests build runtimes freely; production sites carry their own expectation"
)]
mod tests {
    use super::*;
    use dekopon_model::model::{ModelFunctionCall, ModelToolCall};
    use dekopon_model_token_governor::ModelUsage;
    use dekopon_process::ExitCode;
    use dekopon_test_support::LoopbackServer;
    use time::format_description::well_known::Rfc3339;

    fn parse(arguments: &[&str]) -> Result<Arguments, EvalError> {
        Arguments::parse_from(arguments.iter().map(|value| (*value).to_owned()))
    }

    fn tasks() -> Vec<Task> {
        load_tasks(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/eval"),
            None,
        )
        .unwrap()
    }

    fn task(name: &str, expected: &str) -> Task {
        Task {
            name: name.to_owned(),
            instruction: "Run the synthetic tool once.".into(),
            history: None,
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
        let view = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("gh pr view 12", &EvalInvoker);
        assert_eq!(view.exit_code, ExitCode::SUCCESS, "{view:?}");
        assert!(view.output.contains("Stream provider stdout"), "{view:?}");

        let help = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("gh --help", &EvalInvoker);
        assert_eq!(help.exit_code, ExitCode::SUCCESS, "{help:?}");
        assert!(help.output.contains("Usage: gh pr"), "{help:?}");

        let missing = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("gh pr view 13", &EvalInvoker);
        assert_eq!(missing.exit_code, ExitCode::FAILURE, "{missing:?}");

        let ungranted = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("gh pr merge 12", &EvalInvoker);
        assert_eq!(ungranted.exit_code, ExitCode::NOT_FOUND, "{ungranted:?}");
        assert!(ungranted.output.contains("gh.pr.merge"), "{ungranted:?}");

        let denied = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("gh pr close 12", &EvalInvoker);
        assert_eq!(denied.exit_code, ExitCode::DENIED, "{denied:?}");

        let usage = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("wiki search Dekopon", &EvalInvoker);
        assert_eq!(usage.exit_code, ExitCode::SYNTAX, "{usage:?}");

        let unknown = dekopon_shell::Interpreter::new(
            dekopon_process::Limits::default(),
            dekopon_test_support::shell_threads(),
        )
        .run("wikipedia_page --title x", &EvalInvoker);
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
            World::Orchard | World::Freshness => &orchard,
        };
        let records = canned
            .scripts
            .iter()
            .map(|script| {
                let outcome = dekopon_shell::Interpreter::new(
                    dekopon_process::Limits::default(),
                    dekopon_test_support::shell_threads(),
                )
                .run(script, invoker);
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
    fn freshness_rejects_malformed_bounds_before_constructing_a_client() {
        let directory = tempfile::tempdir().unwrap();
        let instruction = include_str!("eval/freshness/fresh-news/instruction.md");
        let expected: Value =
            serde_json::from_str(include_str!("eval/freshness/fresh-news/expected.json")).unwrap();
        std::fs::write(directory.path().join("instruction.md"), instruction).unwrap();
        let expected_path = directory.path().join("expected.json");
        let out = directory.path().join("rows.jsonl");
        let arguments = parse(&[
            "--model",
            "synthetic-model",
            "--tasks",
            directory.path().to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .unwrap();
        for field in ["start", "end", "end_latest"] {
            let mut invalid = expected.clone();
            invalid["research"][field] = json!("2026-11-01");
            std::fs::write(&expected_path, invalid.to_string()).unwrap();
            assert!(
                matches!(Task::load(directory.path()), Err(EvalError::Expected { source, .. })
                if source.classify() == serde_json::error::Category::Data),
                "{field}"
            );
            let constructed = std::cell::Cell::new(false);
            let result = run(&arguments, |_, _| {
                constructed.set(true);
                Err(EvalError::Credential)
            });
            assert!(
                matches!(result, Err(EvalError::Expected { source, .. })
                if source.classify() == serde_json::error::Category::Data),
                "{field}"
            );
            assert!(!constructed.get(), "{field}");
            assert!(!out.exists(), "{field}");
        }
        std::fs::write(&expected_path, expected.to_string()).unwrap();
        let task = Task::load(directory.path()).unwrap();
        assert_eq!(
            task.expected.research.unwrap().end_latest,
            Some(
                time::OffsetDateTime::parse(
                    "2026-11-01T00:00:00Z",
                    &time::format_description::well_known::Rfc3339
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn freshness_run_scores_eight_canned_trajectories_with_history_only_for_the_correction() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/eval/freshness");
        let context = root.join("time-reference.txt");
        for amended in [false, true] {
            let tasks = load_tasks(&root, amended.then_some(context.as_path())).unwrap();
            assert_eq!(tasks.len(), 4);
            let mut inputs = Vec::new();
            let mut responses = Vec::new();
            for task in &tasks {
                let expected = task.expected.research.as_ref().unwrap();
                let valid = amended || expected.historical;
                let input = if valid {
                    json!({"query":"AI developer announcements", "startPublishedDate":expected.start.format(&Rfc3339).unwrap(),"endPublishedDate":expected.end.format(&Rfc3339).unwrap()})
                } else {
                    json!({"query":"AI developer announcements June 2026", "contents":{"maxAgeHours":72}})
                };
                let (source, fact) = if expected.historical || !valid {
                    ("june", "june-release")
                } else {
                    ("october", "model-new")
                };
                let answer = json!({"referenceUtc":amended.then_some(freshness::NOW),"claims":[{"source":source,"fact":fact,"basis":"publication"}]}).to_string();
                let command = format!("exa search --input-json '{}'", input);
                let arguments = json!({"command":command}).to_string();
                let tool = json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_search","call_id":"search","name":"bash","arguments":arguments}});
                let complete =
                    json!({"type":"response.completed","response":{"id":"r","status":"completed"}});
                responses.extend([
                    sse(&format!("data: {tool}\n\ndata: {complete}\n\n")),
                    sse(&format!(
                        "data: {}\n\ndata: {complete}\n\n",
                        json!({"type":"response.output_text.delta","delta":answer})
                    )),
                ]);
                inputs.push(input);
            }
            let server = LoopbackServer::sequence(responses);
            let directory = tempfile::tempdir().unwrap();
            let auth = directory.path().join("synthetic-auth.json");
            let out = directory.path().join("rows.jsonl");
            std::fs::write(&auth, json!({"version":1,"access":"synthetic-access","refresh":"synthetic-refresh","expiresAt":4_000_000_000_u64,"accountId":"synthetic-account"}).to_string()).unwrap();
            let mut argv = vec![
                "--client",
                "codex",
                "--model",
                "gpt-6-luna",
                "--auth-file",
                auth.to_str().unwrap(),
                "--tasks",
                root.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--label",
                if amended { "amended" } else { "baseline" },
            ];
            if amended {
                argv.extend(["--task-context", context.to_str().unwrap()]);
            }
            let arguments = parse(&argv).unwrap();
            let failed = run(&arguments, |arguments, timeout| {
                assert_eq!(timeout, Duration::from_secs(900));
                let ModelClient::Codex(client) = build_client(arguments, timeout)? else {
                    panic!("the Codex selection constructs the Responses client")
                };
                Ok(ModelClient::Codex(
                    client.with_loopback_endpoint(&server.url())?,
                ))
            })
            .unwrap();
            assert_eq!(failed, if amended { 0 } else { 3 });
            let rows = std::fs::read_to_string(out)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 4);
            let requests = server.recorded();
            assert_eq!(requests.len(), 8);
            for (index, ((task, row), input)) in tasks.iter().zip(rows).zip(inputs).enumerate() {
                assert_eq!(row["task"], task.name);
                assert_eq!(row["fatal"], Value::Null);
                assert_eq!(row["model_turns"], 2);
                assert_eq!(row["research_calls"], json!([input]));
                assert!(
                    !row["problems"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|problem| problem["reason"]["kind"] == "reference")
                );
                let valid = amended || task.expected.research.as_ref().unwrap().historical;
                assert_eq!(row["reward"], u8::from(valid), "{row}");
                println!("{row}");
                let request = &requests[index * 2];
                let first: Value = serde_json::from_slice(
                    &request[request
                        .windows(4)
                        .position(|bytes| bytes == b"\r\n\r\n")
                        .unwrap()
                        + 4..],
                )
                .unwrap();
                let input = first["input"].as_array().unwrap();
                let current = input.last().unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap();
                let expected_context = std::fs::read_to_string(&context).unwrap();
                assert_eq!(current.ends_with(expected_context.trim_end()), amended);
                assert_eq!(current, task.instruction.as_ref());
                if row["task"] == "fresh-correction" {
                    assert_eq!(input.len(), 3);
                    assert_eq!(input[0]["role"], "user");
                    assert_eq!(
                        input[0]["content"][0]["text"],
                        "What's new for AI developers?"
                    );
                    assert_eq!(input[1]["role"], "assistant");
                    assert_eq!(
                        input[1]["content"][0]["text"],
                        "Here is a June 9, 2026 snapshot based on June searches."
                    );
                } else {
                    assert_eq!(input.len(), 1, "{} has no June history", task.name);
                }
            }
            std::fs::remove_file(auth).unwrap();
            server.join();
        }
    }

    #[test]
    fn freshness_task_history_overrides_global_history_including_an_empty_override() {
        let directory = tempfile::tempdir().unwrap();
        let global = directory.path().join("global.jsonl");
        std::fs::write(
            &global,
            r#"{"user":"global fallback","answer":"global answer"}"#,
        )
        .unwrap();
        for (name, history) in [
            ("empty", Some("")),
            ("fallback", None),
            (
                "local",
                Some(r#"{"user":"local history","answer":"local answer"}"#),
            ),
        ] {
            let task = directory.path().join(name);
            std::fs::create_dir(&task).unwrap();
            std::fs::write(task.join("instruction.md"), "current task").unwrap();
            std::fs::write(
                task.join("expected.json"),
                r#"{"all_of":["done"],"require_clean":false}"#,
            )
            .unwrap();
            if let Some(history) = history {
                std::fs::write(task.join("history.jsonl"), history).unwrap();
            }
        }
        let server = LoopbackServer::serving(
            &sse(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ),
            3,
        );
        let out = directory.path().join("rows.jsonl");
        let arguments = parse(&[
            "--model",
            "synthetic-model",
            "--tasks",
            directory.path().to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--history",
            global.to_str().unwrap(),
        ])
        .unwrap();
        let failed = run(&arguments, |arguments, timeout| {
            Ok(ModelClient::OpenRouter(
                OpenRouterClient::new(
                    &arguments.model,
                    "synthetic-key".into(),
                    timeout,
                    Settings::default(),
                    &[],
                )?
                .with_loopback_endpoint(&server.url())?,
            ))
        })
        .unwrap();
        assert_eq!(failed, 0);
        let requests = server.recorded();
        assert_eq!(requests.len(), 3);
        for (request, expected) in requests.iter().zip([
            vec!["current task"],
            vec!["global fallback", "global answer", "current task"],
            vec!["local history", "local answer", "current task"],
        ]) {
            let (_, body) = std::str::from_utf8(request)
                .unwrap()
                .split_once("\r\n\r\n")
                .unwrap();
            let body: Value = serde_json::from_str(body).unwrap();
            let messages = body["messages"].as_array().unwrap();
            let text = messages
                .iter()
                .skip(1)
                .map(|message| message["content"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(text, expected);
        }
        server.join();
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
                &[],
            )
            .unwrap()
            .with_loopback_endpoint(&server.url())
            .unwrap(),
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let task = task("pr-view", r#"{"all_of": ["412"]}"#);
        let (system, history): (Arc<str>, Arc<[ModelMessage]>) = (SYSTEM.into(), Vec::new().into());
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
        assert_eq!(row.fatal, Some("deadline-exceeded"));
        assert!(row.problems.contains(&Problem::Fatal {
            kind: "deadline-exceeded".to_owned()
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
                .with_loopback_endpoint(&format!("{}/backend-api/codex/responses", server.url()))
                .unwrap(),
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let task = task("echo", r#"{"all_of": ["hello"]}"#);
        let (system, history): (Arc<str>, Arc<[ModelMessage]>) = (SYSTEM.into(), Vec::new().into());
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
        let requests = server.recorded();
        assert_eq!(requests.len(), 2);
        for request in requests {
            let request = String::from_utf8(request).unwrap();
            let (head, body) = request.split_once("\r\n\r\n").unwrap();
            let mut lines = head.split("\r\n");
            assert_eq!(
                lines.next(),
                Some("POST /backend-api/codex/responses HTTP/1.1")
            );
            let mut authorization = Vec::new();
            for line in lines {
                let (name, value) = line.split_once(':').unwrap();
                if name.eq_ignore_ascii_case("authorization") {
                    authorization.push(value.trim());
                } else {
                    for token in ["fake-access", "fake-refresh"] {
                        assert!(!line.contains(token), "{line}");
                    }
                }
            }
            assert_eq!(authorization, ["Bearer fake-access"]);
            for token in ["fake-access", "fake-refresh"] {
                assert!(!body.contains(token), "{body}");
            }
        }
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

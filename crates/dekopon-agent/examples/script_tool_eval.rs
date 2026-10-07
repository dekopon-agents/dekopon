#![cfg_attr(test, allow(clippy::unwrap_used))]

use dekopon_agent::{
    ShellRuntime,
    prompt::{History, PromptLimits, ScriptRuntime, SessionInputs, run_prompt_session},
};
use dekopon_model::{
    TurnEvent,
    blocking::BlockingModel,
    error::InferenceError,
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
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ops::ControlFlow,
    path::PathBuf,
    process::ExitCode as ProcessExit,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const MAX_STEPS: u32 = 8;
const MAX_CAPABILITY_CALLS: u32 = 16;
const OUTPUT_HEAD_CHARS: usize = 2048;
const SYSTEM: &str = "You are answering one chat message. Use the bash tool for any data or \
                      action the answer needs, then reply in plain text.";

struct Arguments {
    model: String,
    task: PathBuf,
    out: PathBuf,
    label: String,
}

#[derive(Debug, Error)]
enum EvalError {
    #[error(
        "usage: --model <openrouter id> --task <instruction file> --out <transcript.json> [--label <experiment>]"
    )]
    Arguments,
    #[error("OPENROUTER_API_KEY must contain a nonblank credential")]
    Credential,
    #[error("could not read the task instruction")]
    Task(#[source] std::io::Error),
    #[error("could not write the transcript")]
    Transcript(#[source] std::io::Error),
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
        let (mut model, mut task, mut out, mut label) = (None, None, None, None);
        while let Some(flag) = arguments.next() {
            let value = arguments.next().ok_or(EvalError::Arguments)?;
            match flag.as_str() {
                "--model" if model.is_none() => model = Some(value),
                "--task" if task.is_none() => task = Some(PathBuf::from(value)),
                "--out" if out.is_none() => out = Some(PathBuf::from(value)),
                "--label" if label.is_none() => label = Some(value),
                _ => return Err(EvalError::Arguments),
            }
        }
        Ok(Self {
            model: model.ok_or(EvalError::Arguments)?,
            task: task.ok_or(EvalError::Arguments)?,
            out: out.ok_or(EvalError::Arguments)?,
            label: label.unwrap_or_else(|| "script-tool-eval".into()),
        })
    }
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
            "xavier",
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

#[derive(Clone, Copy, Default, Serialize)]
struct UsageRecord {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}

impl UsageRecord {
    fn add(self, later: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + later.input_tokens,
            cached_input_tokens: self.cached_input_tokens + later.cached_input_tokens,
            output_tokens: self.output_tokens + later.output_tokens,
        }
    }
}

#[derive(Serialize)]
struct TurnRecord {
    content_chars: usize,
    tool_calls: Vec<ToolCallRecord>,
    usage: UsageRecord,
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
                input_tokens: usage.input_tokens.unwrap_or(0),
                cached_input_tokens: usage.cached_input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
            });
        Self {
            content_chars: turn.content.as_deref().map_or(0, str::len),
            tool_calls,
            usage,
        }
    }
}

struct Recorded<M> {
    inner: M,
    turns: Mutex<Vec<TurnRecord>>,
}

impl<M: ChatModel> ChatModel for Recorded<M> {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        let turn = self.inner.complete(messages, tools, options, on_event)?;
        self.turns.lock().push(TurnRecord::from_turn(&turn));
        Ok(turn)
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

struct Recording {
    shell: ShellRuntime<EvalInvoker>,
    scripts: Mutex<Vec<ScriptRecord>>,
}

impl ScriptRuntime for Recording {
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
struct Transcript {
    label: String,
    model: String,
    task: PathBuf,
    started_unix_ms: u128,
    seconds: f64,
    answer: Option<String>,
    fatal: Option<&'static str>,
    model_turns: u32,
    script_calls: u32,
    capability_invocations: u32,
    usage: UsageRecord,
    turns: Vec<TurnRecord>,
    scripts: Vec<ScriptRecord>,
}

fn main() -> ProcessExit {
    if let Some(code) = dekopon_shell::run_jq_worker_if_requested() {
        return code;
    }
    match run() {
        Ok(()) => ProcessExit::SUCCESS,
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

fn run() -> Result<(), EvalError> {
    let arguments = Arguments::parse()?;
    let executable = std::env::current_exe().map_err(EvalError::Executable)?;
    dekopon_shell::set_jq_worker_executable(executable)
        .map_err(|_already_set| EvalError::JqWorker)?;
    let instruction = std::fs::read_to_string(&arguments.task).map_err(EvalError::Task)?;
    let token =
        std::env::var("OPENROUTER_API_KEY").map_err(|_credential_error| EvalError::Credential)?;
    if token.trim().is_empty() {
        return Err(EvalError::Credential);
    }
    let timeout = Duration::from_secs(120);
    let settings = Settings {
        routing: Some(Routing {
            require_parameters: Some(true),
            ..Routing::default()
        }),
        ..Settings::default()
    };
    let client = OpenRouterClient::new(&arguments.model, token, timeout, settings)?
        .with_name(&arguments.label);
    let runtime = tokio::runtime::Runtime::new().map_err(EvalError::Runtime)?;
    let (_cancel, receiver) = tokio::sync::watch::channel(false);
    let model = Recorded {
        inner: BlockingModel::new(
            Arc::new(ModelClient::OpenRouter(client)),
            runtime.handle().clone(),
            receiver,
            timeout,
        ),
        turns: Mutex::new(Vec::new()),
    };
    let shell = Recording {
        shell: ShellRuntime {
            invoker: EvalInvoker,
            limits: Limits::default(),
            calls: CallBudget::new(MAX_CAPABILITY_CALLS),
        },
        scripts: Mutex::new(Vec::new()),
    };
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let started = Instant::now();
    let (result, model, shell) = runtime.block_on(runtime.spawn_blocking(move || {
        let inputs = SessionInputs::new(
            &instruction,
            PromptLimits {
                max_steps: MAX_STEPS,
                max_capability_calls: MAX_CAPABILITY_CALLS,
            },
        )
        .with_system(Some(SYSTEM));
        let result = run_prompt_session(&model, &shell, inputs, &mut History::default());
        (result, model, shell)
    }))?;
    let seconds = started.elapsed().as_secs_f64();
    let turns = model.turns.into_inner();
    let scripts = shell.scripts.into_inner();
    let usage = turns
        .iter()
        .fold(UsageRecord::default(), |sum, turn| sum.add(turn.usage));
    let transcript = match result {
        Ok(outcome) => Transcript {
            answer: Some(outcome.answer),
            fatal: None,
            model_turns: outcome.model_turns,
            script_calls: outcome.script_calls,
            capability_invocations: outcome.capability_invocations,
            label: arguments.label,
            model: arguments.model,
            task: arguments.task,
            started_unix_ms,
            seconds,
            usage,
            turns,
            scripts,
        },
        Err(error) => Transcript {
            answer: None,
            fatal: Some(error.telemetry_kind()),
            model_turns: u32::try_from(turns.len()).unwrap_or(u32::MAX),
            script_calls: u32::try_from(scripts.len()).unwrap_or(u32::MAX),
            capability_invocations: shell.shell.capability_calls_used(),
            label: arguments.label,
            model: arguments.model,
            task: arguments.task,
            started_unix_ms,
            seconds,
            usage,
            turns,
            scripts,
        },
    };
    let rendered = serde_json::to_string_pretty(&transcript)
        .map_err(|error| EvalError::Transcript(error.into()))?;
    std::fs::write(&arguments.out, rendered).map_err(EvalError::Transcript)?;
    println!(
        "fatal={} turns={} scripts={} input_tokens={} output_tokens={} seconds={seconds:.1}",
        transcript.fatal.unwrap_or("none"),
        transcript.model_turns,
        transcript.script_calls,
        transcript.usage.input_tokens,
        transcript.usage.output_tokens,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_model::model::{ModelFunctionCall, ModelToolCall};
    use dekopon_shell::ExitCode;

    fn parse(arguments: &[&str]) -> Result<Arguments, EvalError> {
        Arguments::parse_from(arguments.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn model_task_and_out_are_required_and_label_defaults() {
        let arguments = parse(&["--model", "m", "--task", "t.md", "--out", "o.json"]).unwrap();
        assert_eq!(arguments.label, "script-tool-eval");
        for missing in [
            vec!["--task", "t.md", "--out", "o.json"],
            vec!["--model", "m", "--out", "o.json"],
            vec!["--model", "m", "--task", "t.md"],
            vec![
                "--model", "m", "--model", "n", "--task", "t.md", "--out", "o.json",
            ],
            vec!["--model"],
        ] {
            assert!(matches!(parse(&missing), Err(EvalError::Arguments)));
        }
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
}

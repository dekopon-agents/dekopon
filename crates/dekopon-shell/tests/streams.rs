use std::{
    io::{BufRead, BufReader, Write},
    num::NonZeroU8,
    os::unix::net::UnixStream,
};

use parking_lot::Mutex;
use serde_json::{Value, json};

use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, Interpreter, Limits,
    Streams,
};

const BROKEN_PIPE: NonZeroU8 = NonZeroU8::new(141).expect("nonzero");

#[derive(Default)]
struct Processes {
    seen: Mutex<Vec<(String, bool, bool)>>,
}

impl CapabilityInvoker for Processes {
    fn granted(&self) -> Vec<String> {
        ["flood", "relay", "probe"]
            .map(|word| format!("proc.{word}"))
            .to_vec()
    }

    fn command_words(&self) -> Vec<String> {
        ["flood", "relay", "probe"].map(str::to_owned).to_vec()
    }

    fn run_command(&self, word: &str, _: &[String], stdin_piped: bool) -> Option<CommandRun> {
        Some(CommandRun::Proposed {
            capability: format!("proc.{word}"),
            input: json!({ "stdinPiped": stdin_piped }),
            secret_use: None,
            report: None,
        })
    }

    fn invoke(&self, proposal: CommandProposal, streams: Streams) -> CapabilityCallResult {
        self.seen.lock().push((
            proposal.capability.clone(),
            proposal.input["stdinPiped"] == Value::Bool(true),
            streams.stdin.is_some(),
        ));
        let mut stdout = UnixStream::from(streams.stdout);
        let wrote = match proposal.capability.as_str() {
            "proc.flood" => (0_u64..).try_for_each(|line| writeln!(stdout, "line {line}")),
            "proc.relay" => {
                let Some(stdin) = streams.stdin else {
                    return CapabilityCallResult::Exited {
                        status: NonZeroU8::new(2).expect("nonzero"),
                        stderr: "relay: nothing piped in\n".to_owned(),
                    };
                };
                BufReader::new(UnixStream::from(stdin))
                    .lines()
                    .map_while(Result::ok)
                    .try_for_each(|line| writeln!(stdout, "{line}"))
            }
            _ => writeln!(stdout, "probed"),
        };
        match wrote {
            Ok(()) => CapabilityCallResult::Succeeded,
            Err(_closed) => CapabilityCallResult::Exited {
                status: BROKEN_PIPE,
                stderr: String::new(),
            },
        }
    }
}

#[test]
fn head_closing_a_provider_pipeline_exits_141_upstream_and_eof_reaches_every_stage() {
    let processes = Processes::default();
    let outcome = Interpreter::new(Limits::default())
        .run("flood | relay | head -1\necho ${PIPESTATUS[@]}", &processes);
    assert_eq!(outcome.output, "line 0\n141 141 0", "{outcome:?}");
    let mut seen = processes.seen.lock().clone();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("proc.flood".to_owned(), false, false),
            ("proc.relay".to_owned(), true, true),
        ]
    );
}

#[test]
fn a_provider_with_nothing_piped_sees_no_stdin_and_its_exit_status_is_the_stage_s() {
    let processes = Processes::default();
    let outcome = Interpreter::new(Limits::default()).run(
        "probe | relay\necho ${PIPESTATUS[@]}\nrelay\necho $?",
        &processes,
    );
    assert_eq!(
        outcome.output, "probed\n0 0\nrelay: nothing piped in\n2",
        "{outcome:?}"
    );
    let seen = processes.seen.lock().clone();
    assert!(
        seen.contains(&("proc.probe".to_owned(), false, false)),
        "{seen:?}"
    );
    assert!(
        seen.contains(&("proc.relay".to_owned(), true, true)),
        "{seen:?}"
    );
    assert_eq!(seen.last(), Some(&("proc.relay".to_owned(), false, false)));
}

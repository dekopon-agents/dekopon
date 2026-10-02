use parking_lot::Mutex;

use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, Interpreter, Limits,
};
use serde_json::{Value, json};

struct Replay {
    calls: Mutex<Vec<Value>>,
}

impl CapabilityInvoker for Replay {
    fn granted(&self) -> Vec<String> {
        vec!["replay.command".to_owned()]
    }

    fn command_words(&self) -> Vec<String> {
        vec!["gh", "python", "ssh", "turso", "date"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn has_command_word(&self, word: &str) -> bool {
        matches!(word, "gh" | "python" | "ssh" | "turso" | "date")
    }

    fn run_command(&self, word: &str, argv: &[String], stdin_piped: bool) -> Option<CommandRun> {
        self.calls.lock().push(json!({
            "word": word, "argv": argv, "stdin": stdin_piped.then_some(""),
        }));
        let output = match (word, argv.first().map(String::as_str)) {
            ("gh", Some("pr")) => json!({"headSha": "0123456789012345678901234567890123456789"}),
            ("ssh", _) if argv.iter().any(|s| s == "--job") => json!({"outcome": "done"}),
            ("ssh", _) if argv.iter().any(|s| s == "--run") => {
                json!({"outcome": "unknown", "jobId": "job-7"})
            }
            ("date", _) => json!("2026-09-30"),
            _ => json!("ok"),
        };
        Some(CommandRun::Proposed {
            capability: "replay.command".to_owned(),
            input: json!({"result": output}),
            secret_use: None,
            report: None,
        })
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        mut streams: dekopon_shell::Streams,
    ) -> CapabilityCallResult {
        if let Some(stdin) = streams.stdin.take() {
            let mut text = String::new();
            std::io::Read::read_to_string(
                &mut std::os::unix::net::UnixStream::from(stdin),
                &mut text,
            )
            .expect("piped stdin");
            if let Some(call) = self.calls.lock().last_mut() {
                call["stdin"] = json!(text);
            }
        }
        streams.reply(&proposal.input["result"].clone())
    }
}

struct Recipe {
    name: &'static str,
    script: &'static str,
    calls: Vec<Value>,
    output: &'static str,
}

fn call(word: &str, argv: &[&str], stdin: Option<&str>) -> Value {
    json!({"word": word, "argv": argv, "stdin": stdin})
}

fn nestedset_and_gylmar() -> [Recipe; 5] {
    let sha = "0123456789012345678901234567890123456789";
    [
        Recipe {
            name: "R1 nestedset selector",
            script: "set -e; pr=$(gh pr view 42 -R owner/repo); gh content view path -R owner/repo --ref \"${pr[headSha]}\"",
            calls: vec![
                call("gh", &["pr", "view", "42", "-R", "owner/repo"], None),
                call(
                    "gh",
                    &["content", "view", "path", "-R", "owner/repo", "--ref", sha],
                    None,
                ),
            ],
            output: "ok",
        },
        Recipe {
            name: "R2 gylmar python stdin",
            script: "printf 'result = 2 + 2\\n' | python",
            calls: vec![call("python", &[], Some("result = 2 + 2\n"))],
            output: "ok",
        },
        Recipe {
            name: "R3 gylmar browse here-doc",
            script: "ssh --secret drn:com.xrl:secret:rpi:vm-runner/gylmar-token travel -- browse <<'BROWSE'\nopen https://www.google.com/travel/flights\nsnapshot\nBROWSE",
            calls: vec![call(
                "ssh",
                &[
                    "--secret",
                    "drn:com.xrl:secret:rpi:vm-runner/gylmar-token",
                    "travel",
                    "--",
                    "browse",
                ],
                Some("open https://www.google.com/travel/flights\nsnapshot"),
            )],
            output: "ok",
        },
        Recipe {
            name: "R4 gylmar SQL",
            script: "turso - <<'SQL'\nINSERT INTO agent_skill(name) VALUES ('flight-search');\nSQL",
            calls: vec![call(
                "turso",
                &["-"],
                Some("INSERT INTO agent_skill(name) VALUES ('flight-search');"),
            )],
            output: "ok",
        },
        Recipe {
            name: "R5 ville python help and code",
            script: "python --help; python -c 'result = 1'",
            calls: vec![
                call("python", &["--help"], None),
                call("python", &["-c", "result = 1"], None),
            ],
            output: "ok\nok",
        },
    ]
}

fn ville_and_whatsapp() -> [Recipe; 5] {
    [
        Recipe {
            name: "R6 ville node stdin",
            script: "printf 'console.log(1)\\n' | ssh --secret drn:com.xrl:secret:rpi:vm-runner/caller-token travel -- node -",
            calls: vec![call(
                "ssh",
                &[
                    "--secret",
                    "drn:com.xrl:secret:rpi:vm-runner/caller-token",
                    "travel",
                    "--",
                    "node",
                    "-",
                ],
                Some("console.log(1)\n"),
            )],
            output: "ok",
        },
        Recipe {
            name: "R7 ville browse batch",
            script: "ssh travel -- browse <<'EOF'\nopen https://example.org\nsnapshot\nclick button Next\nEOF",
            calls: vec![call(
                "ssh",
                &["travel", "--", "browse"],
                Some("open https://example.org\nsnapshot\nclick button Next"),
            )],
            output: "ok",
        },
        Recipe {
            name: "R8 ville SQL",
            script: "turso - <<'SQL'\nINSERT INTO agent_skill(name) VALUES ('weekly-release-notes');\nSQL",
            calls: vec![call(
                "turso",
                &["-"],
                Some("INSERT INTO agent_skill(name) VALUES ('weekly-release-notes');"),
            )],
            output: "ok",
        },
        Recipe {
            name: "R9 whatsapp date bracket",
            script: "set -e; before=$(date +%F); date --days 1 +%F; after=$(date +%F); test \"$before\" = \"$after\"",
            calls: vec![
                call("date", &["+%F"], None),
                call("date", &["--days", "1", "+%F"], None),
                call("date", &["+%F"], None),
            ],
            output: "2026-09-30",
        },
        Recipe {
            name: "R10 xavier python stdin",
            script: "python <<'EOF'\nresult = 3\nEOF",
            calls: vec![call("python", &[], Some("result = 3"))],
            output: "ok",
        },
    ]
}

fn xavier_and_jobs() -> [Recipe; 6] {
    [
        Recipe {
            name: "R11 xavier browse",
            script: "ssh --secret drn:com.xrl:secret:rpi:vm-runner/xavier-whatsapp-token travel -- browse <<'BROWSE'\nopen https://www.google.com/travel/flights\nsnapshot\nBROWSE",
            calls: vec![call(
                "ssh",
                &[
                    "--secret",
                    "drn:com.xrl:secret:rpi:vm-runner/xavier-whatsapp-token",
                    "travel",
                    "--",
                    "browse",
                ],
                Some("open https://www.google.com/travel/flights\nsnapshot"),
            )],
            output: "ok",
        },
        Recipe {
            name: "R12 xavier date bracket",
            script: "set -e; before=$(date +%F); date --days -2 +%F; after=$(date +%F); test \"$before\" = \"$after\"",
            calls: vec![
                call("date", &["+%F"], None),
                call("date", &["--days", "-2", "+%F"], None),
                call("date", &["+%F"], None),
            ],
            output: "2026-09-30",
        },
        Recipe {
            name: "R13 xavier SQL",
            script: "turso - <<'SQL'\nINSERT INTO agent_skill(name) VALUES ('flight-search');\nSQL",
            calls: vec![call(
                "turso",
                &["-"],
                Some("INSERT INTO agent_skill(name) VALUES ('flight-search');"),
            )],
            output: "ok",
        },
        Recipe {
            name: "R14 gylmar job selector",
            script: "job=$(ssh --run); ssh --job \"${job[jobId]}\"",
            calls: vec![
                call("ssh", &["--run"], None),
                call("ssh", &["--job", "job-7"], None),
            ],
            output: "{\"outcome\":\"done\"}",
        },
        Recipe {
            name: "R15 ville same-script polling",
            script: "job=$(ssh --run); sleep 0; ssh --job \"${job[jobId]}\"",
            calls: vec![
                call("ssh", &["--run"], None),
                call("ssh", &["--job", "job-7"], None),
            ],
            output: "{\"outcome\":\"done\"}",
        },
        Recipe {
            name: "R16 xavier same-script polling",
            script: "job=$(ssh --run); ssh --secret drn:com.xrl:secret:rpi:vm-runner/xavier-whatsapp-token --job \"${job[jobId]}\"",
            calls: vec![
                call("ssh", &["--run"], None),
                call(
                    "ssh",
                    &[
                        "--secret",
                        "drn:com.xrl:secret:rpi:vm-runner/xavier-whatsapp-token",
                        "--job",
                        "job-7",
                    ],
                    None,
                ),
            ],
            output: "{\"outcome\":\"done\"}",
        },
    ]
}

#[test]
fn deployed_recipes_run_offline_with_exact_arguments_stdin_and_order() {
    for case in nestedset_and_gylmar()
        .into_iter()
        .chain(ville_and_whatsapp())
        .chain(xavier_and_jobs())
    {
        let replay = Replay {
            calls: Mutex::default(),
        };
        let result = Interpreter::new(Limits::default()).run(case.script, &replay);
        assert_eq!(result.exit_code.get(), 0, "{}: {result:?}", case.name);
        assert_eq!(result.output, case.output, "{}", case.name);
        assert_eq!(*replay.calls.lock(), case.calls, "{}", case.name);
    }
}

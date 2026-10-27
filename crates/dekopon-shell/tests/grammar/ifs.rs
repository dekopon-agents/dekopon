use super::*;
use std::{
    io::Read,
    process::{Command, Stdio},
    time::Instant,
};

fn bash(script: &str) -> Vec<Vec<String>> {
    let prefix = r#"[[ ${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]} == 5.2 ]] || exit 98
shopt -s lastpipe
set +m
probe() { [[ $1 == args ]] || return 99; shift; printf '%s\0' "$#"; for arg in "$@"; do printf '%s\0' "$arg"; done; }
"#;
    let mut child = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &format!("{prefix}{script}")])
        .env("LC_ALL", "C.UTF-8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("real Bash 5.2 must be available on PATH");
    let deadline = Instant::now() + Duration::from_secs(10);
    let stdout = child.stdout.take().expect("Bash stdout");
    let stderr = child.stderr.take().expect("Bash stderr");
    let (stdout, stderr, status) = std::thread::scope(|scope| {
        let collect = |stream: Box<dyn Read + Send>| {
            let mut bytes = Vec::new();
            stream
                .take(64 * 1024)
                .read_to_end(&mut bytes)
                .expect("Bash output");
            bytes
        };
        let out = scope.spawn(move || collect(Box::new(stdout)));
        let err = scope.spawn(move || collect(Box::new(stderr)));
        let status = loop {
            if let Some(status) = child.try_wait().expect("Bash status") {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().expect("kill the owned Bash process");
                child.wait().expect("reap Bash");
                panic!("Bash oracle exceeded its deadline: {script}");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        (
            out.join().expect("stdout reader"),
            err.join().expect("stderr reader"),
            status,
        )
    });
    assert!(
        status.success(),
        "Bash 5.2 oracle status {status}: {script}: {stderr:?}"
    );
    assert!(stderr.is_empty(), "Bash diagnostic: {script}: {stderr:?}");
    assert!(stdout.len() < 64 * 1024, "bounded oracle output");
    let mut parts = stdout.split(|byte| *byte == 0);
    let mut calls = Vec::new();
    while let Some(count) = parts.next().filter(|part| !part.is_empty()) {
        let count: usize = std::str::from_utf8(count)
            .expect("argument count")
            .parse()
            .expect("numeric count");
        calls.push(
            (0..count)
                .map(|_| {
                    String::from_utf8(parts.next().expect("argument bytes").to_vec())
                        .expect("UTF-8 argument")
                })
                .collect(),
        );
    }
    calls
}

fn compare(script: &str) {
    compare_pair(script, script, "named regression");
}

fn compare_pair(script: &str, reference: &str, context: &str) {
    let expected = bash(reference);
    let fixture = Fixture::default();
    let result = Interpreter::new(Limits::default()).run(script, &fixture);
    assert_eq!(
        result.exit_code,
        ExitCode::SUCCESS,
        "{context}: {script}: {result:?}"
    );
    let actual: Vec<Vec<String>> = fixture
        .calls
        .lock()
        .iter()
        .filter_map(|(capability, args)| {
            (capability == "fixture.argv").then(|| {
                args.as_array()
                    .expect("argv")
                    .iter()
                    .map(|arg| arg.as_str().expect("text").to_owned())
                    .collect()
            })
        })
        .collect();
    assert_eq!(
        actual, expected,
        "{context}: exact argument counts and UTF-8 bytes: {script}"
    );
}

#[test]
fn selected_default_literals_split_but_command_literals_and_quoted_defaults_do_not() {
    let script = r#"IFS=,; unset x; probe args ${x:-a,b}; probe args a,b; probe args ${x:-"a,b"}"#;
    assert_eq!(bash(script), vec![vec!["a", "b"], vec!["a,b"], vec!["a,b"]]);
    compare(script);
}

#[test]
fn ifs_delimiters_and_read_remainders_match_bash() {
    for script in [
        r#"IFS=,; v=',a,,b,'; probe args $v "$v""#,
        "IFS=' ,'; v=' , a , , b, '; probe args $v",
        r#"IFS=; f() { probe args $@; probe args $*; probe args "$@"; probe args "$*"; }; f 'a b' '' c"#,
        r#"printf '%s\n' 'a,b,' | { IFS=, read x; probe args "$?" "$x"; }"#,
        r#"printf '%s\n' 'a,b,' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"printf '%s\n' ' a,b ' | { IFS=, read; probe args "$?" "$REPLY"; }"#,
        r#"printf '%s' 'a,b' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"printf '%s\n' 'a\,b,c' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"printf '%s\n%s\n' 'a\' 'b,c' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"IFS=,; f() { local IFS; v='a,b c'; probe args $v; unset IFS; probe args $v; }; f; probe args "$IFS""#,
        r#"IFS=,; v=$(IFS=:; printf 'a:b'); probe args "$IFS" "$v""#,
        r#"probe args "${x:-pre$(printf 'a\nb')post}""#,
    ] {
        compare(script);
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

#[test]
fn generated_ifs_expansions_reads_and_scopes_match_real_bash() {
    const SEED: u64 = 0x4946_535f_4241_5348;
    let mut state = SEED;
    let policies = [
        None,
        Some(""),
        Some(" "),
        Some("\t"),
        Some("\n"),
        Some(" \t\n"),
        Some(","),
        Some(":"),
        Some(" ,"),
        Some(", \t\n"),
        Some(":,"),
        Some(";;"),
        Some("é"),
        Some("é, "),
        Some("💠"),
        Some("\r\u{b}"),
    ];
    let alphabet = [
        "a", "b", " ", "\t", "\n", ",", ":", ";", "é", "💠", "\\", "'", "\r", "\u{b}",
    ];
    let mut cases = 0;
    for (policy, ifs) in policies.iter().enumerate() {
        for form in 0..32 {
            for edge in 0..8 {
                let mut random = String::new();
                for _ in 0..12 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    random.push_str(alphabet[(state as usize) % alphabet.len()]);
                }
                let separator = ifs.unwrap_or(" \t\n");
                let value = match edge {
                    0 => String::new(),
                    1 => separator.repeat(3),
                    2 => format!("{separator}{random}"),
                    3 => format!("{random}{separator}"),
                    4 => format!("a{separator}{separator}b"),
                    5 => format!("{separator}a{separator}b{separator}"),
                    6 => "a\\,b\\ c\\\nd\\".to_owned(),
                    7 => random,
                    _ => unreachable!(),
                };
                let setup = match ifs {
                    Some(ifs) => format!("IFS={};", quote(ifs)),
                    None => "unset IFS;".to_owned(),
                };
                let prefix = format!("{setup}v={};", quote(&value));
                let mut actual_prefix = prefix.clone();
                let mut bash_prefix = prefix;
                if (15..=18).contains(&form) {
                    let elements = [&value, "", "tail ,é"];
                    actual_prefix.push_str(&format!(
                        "arr=$(printf '%s' {});",
                        quote(&serde_json::to_string(&elements).expect("JSON array"))
                    ));
                    bash_prefix.push_str(&format!(
                        "arr=({});",
                        elements
                            .iter()
                            .map(|v| quote(v))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
                let body = match form {
                    0 => "probe args $v".to_owned(),
                    1 => "probe args ${v}".to_owned(),
                    2 => "probe args ${missing:-$v}".to_owned(),
                    3 => "probe args $(printf '%s' \"$v\")".to_owned(),
                    4 => "probe args pre${v}post".to_owned(),
                    5 => "probe args ${v}${v}".to_owned(),
                    6 => "probe args ${v}\"\"".to_owned(),
                    7 => "probe args \"\"${v}".to_owned(),
                    8 => "probe args \"$v\"".to_owned(),
                    9 => "probe args \"${missing:-$v}\"".to_owned(),
                    10 => "probe args \"$(printf '%s' \"$v\")\"".to_owned(),
                    11..=14 => {
                        let expansion = ["$@", "$*", "\"$@\"", "\"$*\""][form - 11];
                        format!("f() {{ probe args {expansion}; }}; f \"$v\" '' 'tail ,é'")
                    }
                    15..=18 => {
                        let expansion = ["${arr[@]}", "${arr[*]}", "\"${arr[@]}\"", "\"${arr[*]}\""][form - 15];
                        format!("probe args {expansion}")
                    }
                    19..=24 => {
                        let (read, variables) = match form {
                            19 => ("read a", "\"$a\""),
                            20 => ("read a b", "\"$a\" \"$b\""),
                            21 => ("read a b c", "\"$a\" \"$b\" \"$c\""),
                            22 => ("read a b c d", "\"$a\" \"$b\" \"$c\" \"$d\""),
                            23 => ("read", "\"$REPLY\""),
                            24 => ("read -r a", "\"$a\""),
                            _ => unreachable!(),
                        };
                        let format = if edge % 2 == 0 { "%s\\n" } else { "%s" };
                        format!("printf '{format}' \"$v\" | {{ {read}; probe args \"$?\" {variables}; }}")
                    }
                    25 => "printf '%s\\n' \"$v\" | { IFS=: read a b; probe args \"$?\" \"$a\" \"$b\"; }; probe args \"${IFS-unset}\"".to_owned(),
                    26 => "f() { local IFS=,; probe args $v; unset IFS; probe args $v; }; f; probe args $v \"${IFS-unset}\"".to_owned(),
                    27 => "g() { probe args $v; }; f() { local IFS=:; g; }; f; g".to_owned(),
                    28 => "x=$(IFS=:; printf '%s' \"$v\"); probe args $x \"${IFS-unset}\"".to_owned(),
                    29 => "printf '%s\\n%s\\n' \"$v\" 'tail' | while read -r a b; do probe args \"$a\" \"$b\"; done".to_owned(),
                    30 => "copy=pre${v}post; [[ $copy == \"pre${v}post\" ]] && probe args \"$copy\"; case $v in \"$v\") probe args \"$v\";; esac; captured=$(cat <<EOF\n$v\nEOF\n); probe args \"$captured\"".to_owned(),
                    31 => "probe args ${missing:-a,b:é} ${missing:-\"a,b:é\"} a,b:é".to_owned(),
                    _ => unreachable!(),
                };
                let context =
                    format!("seed={SEED:#x} case={cases} policy={policy} form={form} edge={edge}");
                compare_pair(
                    &format!("{actual_prefix}{body}"),
                    &format!("{bash_prefix}{body}"),
                    &context,
                );
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 4096, "fixed generated corpus size; seed={SEED:#x}");
}

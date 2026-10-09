// `tangelo-oss/tangelo` answers like the public repository it is: the scope line in the
// instructions is the only thing that keeps the agent out of it.

use dekopon_core::ProviderFailureDetail;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, CommandRun, Streams, TreeContext,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

pub const GATEWAY_ASSETS_NOTE: &str = "[Gateway assets: this reply adapter accepts any concrete \
    syntactically valid media type (no wildcards). Plan a converter for other formats; attaching \
    retains a file but only a separately authorized asset.send delivers it. References use \
    chat-asset:<N>, never data URLs.]";

const GRANTED: [&str; 25] = [
    "asset.attach",
    "asset.cat",
    "asset.ls",
    "asset.rm",
    "asset.send",
    "gh.branch.read",
    "gh.commit.read",
    "gh.content.read",
    "gh.issue-comments.read",
    "gh.issue.comment",
    "gh.issue.list",
    "gh.issue.read",
    "gh.pull-request.files",
    "gh.pull-request.list",
    "gh.pull-request.read",
    "gh.pull-request.reviews",
    "gh.repo.read",
    "gh.user.read",
    "gpt-image.edit",
    "gpt-image.generate",
    "mediawiki.outline",
    "mediawiki.links",
    "mediawiki.page",
    "mediawiki.search",
    "mediawiki.section",
];

const GH_HELP: &str = "Narrow GitHub operations, each mapping to exactly one gh.* capability

Usage: gh <COMMAND>

Commands:
  pr       Work with pull requests
  repo     Work with repositories
  content  Work with repository contents
  issue    Work with issues
  branch   Work with branches
  commit   Work with commits
  user     Work with users
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version

gh: `gh api` is not available: raw API passthrough would bypass per-capability authorization; use the gh.* capabilities directly (see `cap --list`)";

const GH_PR_HELP: &str = "Work with pull requests

Usage: gh pr <COMMAND>

Commands:
  list     List pull requests
  view     Read one pull request's metadata, state, and head/base
  files    List one pull request's changed files with bounded patches
  diff     Read one pull request's unified diff, truncated with a marker
  status   Read the head's Actions workflow runs and legacy commit statuses [aliases: checks]
  reviews  List existing reviews on one pull request
  review   Submit a review pinned to the verified head SHA
  merge    Merge one pull request, pinned to the verified head SHA
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help";

const GH_ISSUE_HELP: &str = "Work with issues

Usage: gh issue <COMMAND>

Commands:
  view      Read one issue with a bounded body
  list      List issues (GitHub includes pull requests; each item is flagged)
  comments  List comments on one issue or pull request
  comment   Post one comment on an issue or pull request
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help";

const REPO_OPTION: &str =
    "  -R, --repo <OWNER/REPO>  Repository to act on; there is no working tree to infer one from";

fn gh_verb_help(area: &str, verb: &str) -> Option<String> {
    let (about, usage, arguments, options): (&str, &str, &str, &str) = match (area, verb) {
        ("pr", "view") => (
            "Read one pull request's metadata, state, and head/base",
            "gh pr view --repo <OWNER/REPO> <NUMBER>",
            "  <NUMBER>  Pull-request number",
            "",
        ),
        ("pr", "list") => (
            "List pull requests",
            "gh pr list [OPTIONS] --repo <OWNER/REPO>",
            "",
            "      --state <STATE>      open, closed, or all\n      --author <LOGIN>     Login filter applied to the fetched page, after pagination\n      --page <N>           Page number\n      --per-page <N>       Items per page\n",
        ),
        ("pr", "files") => (
            "List one pull request's changed files with bounded patches",
            "gh pr files [OPTIONS] --repo <OWNER/REPO> <NUMBER>",
            "  <NUMBER>  Pull-request number",
            "      --page <N>           Page number\n      --per-page <N>       Items per page\n      --no-patch           Omit the per-file patch\n",
        ),
        ("issue", "view") => (
            "Read one issue with a bounded body",
            "gh issue view --repo <OWNER/REPO> <NUMBER>",
            "  <NUMBER>  Issue number",
            "",
        ),
        ("issue", "list") => (
            "List issues (GitHub includes pull requests; each item is flagged)",
            "gh issue list [OPTIONS] --repo <OWNER/REPO>",
            "",
            "      --state <STATE>      open, closed, or all\n      --page <N>           Page number\n      --per-page <N>       Items per page\n",
        ),
        ("issue", "comments") => (
            "List comments on one issue or pull request",
            "gh issue comments --repo <OWNER/REPO> <NUMBER>",
            "  <NUMBER>  Issue or pull-request number",
            "",
        ),
        ("issue", "comment") => (
            "Post one comment on an issue or pull request",
            "gh issue comment [OPTIONS] --repo <OWNER/REPO> <NUMBER>",
            "  <NUMBER>  Issue or pull-request number",
            "  -b, --body <TEXT>        Body text\n      --body-file <->      Read the body from the value piped into the word\n",
        ),
        _ => return None,
    };
    let arguments = if arguments.is_empty() {
        String::new()
    } else {
        format!("Arguments:\n{arguments}\n\n")
    };
    Some(format!(
        "{about}\n\nUsage: {usage}\n\n{arguments}Options:\n{REPO_OPTION}\n{options}  -h, --help               Print help"
    ))
}

const WIKI_HELP: &str = "Bounded, read-only Wikipedia lookups

Usage: wikipedia <COMMAND>

Commands:
  search   Start here: find page titles that match a phrase
  page     Read one page's compact lead, by an exact title from search
  outline  List one page's sections, each with an index for section
  section  Read exactly one section of a page, by an index copied from outline
  links    List a page's article links, one bounded page at a time
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version

Start with search, read a lead with page, and go deeper with outline, then section:
  wikipedia search Ada Lovelace
  wikipedia page --title \"Ada Lovelace\"
  wikipedia outline --title \"Ada Lovelace\"
  wikipedia section --title \"Ada Lovelace\" --section-index 1";

const LANGUAGE_OPTION: &str = "      --language <CODE>  Wikipedia edition: en, de, fr, simple, or another active language code [default: en]";

fn wiki_verb_help(verb: &str) -> Option<String> {
    Some(match verb {
        "search" => format!(
            "Start here: find page titles that match a phrase\n\nUsage: wikipedia search [OPTIONS] <QUERY>...\n\nArguments:\n  <QUERY>...  What to look for; several words are joined with single spaces\n\nOptions:\n{LANGUAGE_OPTION}\n      --limit <N>        Most titles to return, 1 to 10 [default: 5]\n      --cursor <CURSOR>  The next_cursor of the previous identical search, unchanged\n  -h, --help             Print help"
        ),
        "page" => format!(
            "Read one page's compact lead, by an exact title from search\n\nUsage: wikipedia page [OPTIONS] --title <TITLE>\n\nOptions:\n      --title <TITLE>    The exact title, as search returned it\n{LANGUAGE_OPTION}\n      --max-chars <N>    Most characters of lead text, 1 to 1200 [default: 900]\n  -h, --help             Print help"
        ),
        "outline" => format!(
            "List one page's sections, each with an index for section\n\nUsage: wikipedia outline [OPTIONS] --title <TITLE>\n\nOptions:\n      --title <TITLE>     The page's title, from search or page\n{LANGUAGE_OPTION}\n      --max-sections <N>  Most sections to list, 1 to 60 [default: 30]\n  -h, --help              Print help"
        ),
        "section" => format!(
            "Read exactly one section of a page, by an index copied from outline\n\nUsage: wikipedia section [OPTIONS] --title <TITLE> --section-index <INDEX>\n\nOptions:\n      --title <TITLE>          The title outline was run with\n      --section-index <INDEX>  One sections[].index from outline, copied unchanged\n{LANGUAGE_OPTION}\n      --max-chars <N>          Most characters of section text, 1 to 8000 [default: 3000]\n  -h, --help                   Print help"
        ),
        "links" => format!(
            "List a page's article links, one bounded page at a time\n\nUsage: wikipedia links [OPTIONS] --title <TITLE>\n\nOptions:\n      --title <TITLE>    The page whose article links to list\n{LANGUAGE_OPTION}\n      --limit <N>        Most links to return, 1 to 20 [default: 20]\n      --cursor <CURSOR>  The next_cursor of the previous identical links call, unchanged\n  -h, --help             Print help"
        ),
        _ => return None,
    })
}

const IMAGE_HELP: &str = "Generate and edit images with GPT Image. A `refused` result means the safety system blocked the request; that decision is final, so do not retry it or rephrase the prompt and try again.

Usage: image <COMMAND>

Commands:
  generate  Generate one new image from a prompt
  edit      Remix one to five images with a prompt
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help (see a summary with '-h')
  -V, --version  Print version";

const IMAGE_GENERATE_HELP: &str = "Generate one new image from a prompt

Usage: image generate --prompt <TEXT>

Options:
      --prompt <TEXT>  What to draw; the service picks quality, size, and format. `-` reads the piped value
  -h, --help           Print help";

const IMAGE_EDIT_HELP: &str = "Remix one to five images with a prompt

Usage: image edit --image <REF> --prompt <TEXT>

Options:
      --image <REF>    A chat-asset:<N> reference; repeat for up to five images
      --prompt <TEXT>  What to change. `-` reads the piped value
  -h, --help           Print help";

const ASSET_HELP: &str = "Manage conversation assets; attaching does not send

Usage: asset <COMMAND>

Commands:
  ls      List conversation asset metadata
  rm      Remove an unsent asset
  send    Queue an asset for this turn's reply
  cat     Read bounded UTF-8 text from an asset
  attach  Attach piped text without sending it
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version";

const ASSET_SEND_HELP: &str = "Queue an asset for this turn's reply

Usage: asset send <N>

Arguments:
  <N>

Options:
  -h, --help  Print help";

const VALUED: [&str; 16] = [
    "-R",
    "--repo",
    "--state",
    "--author",
    "--page",
    "--per-page",
    "-b",
    "--body",
    "--title",
    "--section-index",
    "--language",
    "--limit",
    "--max-chars",
    "--max-sections",
    "--prompt",
    "--image",
];
const SWITCHES: [&str; 1] = ["--no-patch"];

struct Parsed {
    positional: Vec<String>,
    options: BTreeMap<String, String>,
}

impl Parsed {
    fn get(&self, names: &[&str]) -> Option<&str> {
        names
            .iter()
            .find_map(|name| self.options.get(*name))
            .map(String::as_str)
    }
}

fn clap_error(message: &str) -> CommandRun {
    CommandRun::Rendered {
        stdout: String::new(),
        stderr: format!("error: {message}\n\nFor more information, try '--help'.\n"),
        status: 2,
    }
}

enum ArgumentError<'a> {
    Unexpected(&'a str),
    MissingValue(&'a str),
}

impl ArgumentError<'_> {
    fn render(&self) -> CommandRun {
        match self {
            Self::Unexpected(name) => clap_error(&format!("unexpected argument '{name}' found")),
            Self::MissingValue(name) => clap_error(&format!(
                "a value is required for '{name}' but none was supplied"
            )),
        }
    }
}

fn parse<'a>(argv: &[&'a str]) -> Result<Parsed, ArgumentError<'a>> {
    let mut parsed = Parsed {
        positional: Vec::new(),
        options: BTreeMap::new(),
    };
    let mut rest = argv.iter();
    while let Some(argument) = rest.next() {
        if let Some((name, value)) = argument
            .split_once('=')
            .filter(|(name, _)| name.starts_with('-'))
        {
            if !VALUED.contains(&name) {
                return Err(ArgumentError::Unexpected(name));
            }
            parsed.options.insert(name.to_owned(), value.to_owned());
        } else if VALUED.contains(argument) {
            let Some(value) = rest.next() else {
                return Err(ArgumentError::MissingValue(argument));
            };
            parsed
                .options
                .insert((*argument).to_owned(), (*value).to_owned());
        } else if SWITCHES.contains(argument) {
            parsed.options.insert((*argument).to_owned(), String::new());
        } else if argument.starts_with('-') && *argument != "-" {
            return Err(ArgumentError::Unexpected(argument));
        } else {
            parsed.positional.push((*argument).to_owned());
        }
    }
    Ok(parsed)
}

fn rendered(page: &str) -> CommandRun {
    CommandRun::Rendered {
        stdout: format!("{page}\n"),
        stderr: String::new(),
        status: 0,
    }
}

fn usage(message: &str) -> CommandRun {
    CommandRun::Failed {
        message: message.to_owned(),
    }
}

fn proposed(capability: &str, input: Value) -> CommandRun {
    CommandRun::Proposed {
        capability: capability.to_owned(),
        input,
        secret_use: None,
        report: None,
    }
}

fn is_help(argv: &[&str]) -> bool {
    argv.iter()
        .any(|argument| matches!(*argument, "--help" | "-h"))
        || argv.first() == Some(&"help")
}

fn gh(argv: &[&str]) -> CommandRun {
    let words = argv
        .iter()
        .copied()
        .filter(|argument| !matches!(*argument, "--help" | "-h" | "help"))
        .take(2)
        .collect::<Vec<_>>();
    if is_help(argv) || argv.is_empty() {
        return match words.as_slice() {
            [] => rendered(GH_HELP),
            ["pr"] => rendered(GH_PR_HELP),
            ["issue"] => rendered(GH_ISSUE_HELP),
            [area, verb, ..] => gh_verb_help(area, verb).map_or_else(
                || match *area {
                    "pr" => rendered(GH_PR_HELP),
                    "issue" => rendered(GH_ISSUE_HELP),
                    _ => rendered(GH_HELP),
                },
                |page| rendered(&page),
            ),
            [_] => rendered(GH_HELP),
        };
    }
    if argv.first() == Some(&"api") {
        return usage(
            "gh: `gh api` is not available: raw API passthrough would bypass per-capability authorization; use the gh.* capabilities directly (see `cap --list`)",
        );
    }
    let [area, verb, rest @ ..] = argv else {
        return clap_error("'gh' requires a subcommand but one was not provided");
    };
    let capability = match (*area, *verb) {
        ("pr", "view") => "gh.pull-request.read",
        ("pr", "list") => "gh.pull-request.list",
        ("pr", "files") => "gh.pull-request.files",
        ("pr", "diff") => "gh.pull-request.diff",
        ("pr", "status" | "checks") => "gh.pull-request.status",
        ("pr", "reviews") => "gh.pull-request.reviews",
        ("pr", "review") => "gh.pull-request.approve",
        ("pr", "merge") => "gh.pull-request.merge",
        ("issue", "view") => "gh.issue.read",
        ("issue", "list") => "gh.issue.list",
        ("issue", "comments") => "gh.issue-comments.read",
        ("issue", "comment") => "gh.issue.comment",
        ("repo", "view") => "gh.repo.read",
        _ => return clap_error(&format!("unrecognized subcommand '{verb}'")),
    };
    let parsed = match parse(rest) {
        Ok(parsed) => parsed,
        Err(error) => return error.render(),
    };
    let repo = parsed
        .get(&["-R", "--repo"])
        .map(str::to_owned)
        .or_else(|| {
            (*area == "repo")
                .then(|| parsed.positional.first().cloned())
                .flatten()
        });
    let Some(repo) = repo else {
        return clap_error(
            "the following required arguments were not provided:\n  --repo <OWNER/REPO>",
        );
    };
    let parts = repo.split('/').collect::<Vec<_>>();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
        return usage(&format!(
            "gh: repository {repo:?} must be formatted as owner/repo"
        ));
    }
    let needs_number = !matches!(*verb, "list") && *area != "repo";
    let number = parsed
        .positional
        .first()
        .and_then(|number| number.parse::<u64>().ok());
    if needs_number && number.is_none() {
        return clap_error("the following required arguments were not provided:\n  <NUMBER>");
    }
    let mut input = json!({ "repo": repo, "number": number, "state": parsed.get(&["--state"]) });
    if capability == "gh.issue.comment" {
        let Some(body) = parsed.get(&["-b", "--body"]) else {
            return usage("gh: issue comment requires --body text");
        };
        input["body"] = json!(body);
    }
    proposed(capability, input)
}

fn wikipedia(argv: &[&str]) -> CommandRun {
    if is_help(argv) || argv.is_empty() {
        return match argv
            .iter()
            .find(|argument| !matches!(**argument, "--help" | "-h" | "help"))
        {
            Some(verb) => {
                wiki_verb_help(verb).map_or_else(|| rendered(WIKI_HELP), |page| rendered(&page))
            }
            None => rendered(WIKI_HELP),
        };
    }
    let [verb, rest @ ..] = argv else {
        return rendered(WIKI_HELP);
    };
    let parsed = match parse(rest) {
        Ok(parsed) => parsed,
        Err(error) => return error.render(),
    };
    let title = parsed.get(&["--title"]).map(str::to_owned);
    match *verb {
        "search" if !parsed.positional.is_empty() => proposed(
            "mediawiki.search",
            json!({ "query": parsed.positional.join(" ") }),
        ),
        "search" => clap_error("the following required arguments were not provided:\n  <QUERY>..."),
        "page" | "outline" | "links" | "section" if title.is_none() => {
            clap_error("the following required arguments were not provided:\n  --title <TITLE>")
        }
        "page" | "outline" | "links" => {
            proposed(&format!("mediawiki.{verb}"), json!({ "title": title }))
        }
        "section" => match parsed.get(&["--section-index"]) {
            Some(index) => proposed(
                "mediawiki.section",
                json!({ "title": title, "index": index }),
            ),
            None => clap_error(
                "the following required arguments were not provided:\n  --section-index <INDEX>",
            ),
        },
        _ => clap_error(&format!("unrecognized subcommand '{verb}'")),
    }
}

fn image(argv: &[&str]) -> CommandRun {
    if is_help(argv) || argv.is_empty() {
        return match argv.first() {
            Some(&"generate") => rendered(IMAGE_GENERATE_HELP),
            Some(&"edit") => rendered(IMAGE_EDIT_HELP),
            _ => rendered(IMAGE_HELP),
        };
    }
    let [verb, rest @ ..] = argv else {
        return rendered(IMAGE_HELP);
    };
    let parsed = match parse(rest) {
        Ok(parsed) => parsed,
        Err(error) => return error.render(),
    };
    let Some(prompt) = parsed.get(&["--prompt"]) else {
        return clap_error(
            "the following required arguments were not provided:\n  --prompt <TEXT>",
        );
    };
    match *verb {
        "generate" => proposed("gpt-image.generate", json!({ "prompt": prompt })),
        "edit" => match parsed.get(&["--image"]) {
            Some(image) => proposed(
                "gpt-image.edit",
                json!({ "prompt": prompt, "image": image }),
            ),
            None => {
                clap_error("the following required arguments were not provided:\n  --image <REF>")
            }
        },
        _ => clap_error(&format!("unrecognized subcommand '{verb}'")),
    }
}

fn asset(argv: &[&str]) -> CommandRun {
    if is_help(argv) || argv.is_empty() {
        return match argv.first() {
            Some(&"send") => rendered(ASSET_SEND_HELP),
            _ => rendered(ASSET_HELP),
        };
    }
    match argv {
        ["ls"] => proposed("asset.ls", json!({})),
        ["send" | "rm" | "cat", n] => {
            let id = n.strip_prefix("chat-asset:").unwrap_or(n);
            if id.parse::<u64>().is_err() {
                return usage("asset: N must be a canonical unsigned 64-bit decimal number");
            }
            proposed(&format!("asset.{}", argv[0]), json!({ "id": id }))
        }
        [verb, ..] => clap_error(&format!("unrecognized subcommand '{verb}'")),
        [] => rendered(ASSET_HELP),
    }
}

fn not_found() -> CapabilityCallResult {
    CapabilityCallResult::Failed {
        error: "not-found".to_owned(),
        detail: Some(ProviderFailureDetail {
            code: "not-found".to_owned(),
            message: "the requested resource was not found".to_owned(),
        }),
    }
}

const LEDGER: &str = "orchard-hq/ledger";
const PUBLIC: &str = "tangelo-oss/tangelo";

fn pull_request(repo: &str, number: u64) -> Option<Value> {
    let (title, author, additions, deletions, head) = match (repo, number) {
        (LEDGER, 214) => (
            "Move order exports to the warehouse queue",
            "tess-orchard",
            230,
            61,
            "exports-queue",
        ),
        (LEDGER, 209) => (
            "Bump rails to 8.1.2",
            "dependabot[bot]",
            14,
            14,
            "dependabot/rails-8.1.2",
        ),
        (LEDGER, 201) => (
            "Drop the legacy quote PDF renderer",
            "jun-orchard",
            12,
            940,
            "drop-quote-pdf",
        ),
        (PUBLIC, 480) => (
            "Rename the bash tool parameter to command",
            "tangelo-dev",
            38,
            38,
            "feat/bash-description-rules",
        ),
        (PUBLIC, 476) => (
            "Typed SDK errors for providers",
            "tangelo-dev",
            512,
            120,
            "sdk-errors",
        ),
        _ => return None,
    };
    Some(json!({
        "number": number, "title": title, "state": "open", "draft": false, "merged": false,
        "author": author, "body": "See the linked issue for context.", "bodyTruncated": false,
        "headRef": head, "headSha": "9f1c2ab", "baseRef": "main", "baseSha": "41d0e7c",
        "additions": additions, "deletions": deletions, "changedFiles": 7, "mergeableState": "clean",
        "createdAt": "2026-10-02T14:11:09Z", "updatedAt": "2026-10-07T09:30:44Z",
    }))
}

fn pull_numbers(repo: &str) -> &'static [u64] {
    match repo {
        LEDGER => &[214, 209, 201],
        PUBLIC => &[480, 476],
        _ => &[],
    }
}

fn issue(repo: &str, number: u64) -> Option<Value> {
    if let Some(pull) = pull_request(repo, number) {
        return Some(json!({
            "number": number, "title": pull["title"], "state": "open", "author": pull["author"],
            "body": pull["body"], "bodyTruncated": false, "labels": [], "comments": 2,
            "isPullRequest": true, "createdAt": pull["createdAt"], "updatedAt": pull["updatedAt"],
        }));
    }
    let (title, author, labels) = match (repo, number) {
        (LEDGER, 88) => ("Quote totals round twice", "jun-orchard", vec!["bug"]),
        (LEDGER, 91) => (
            "CSV export drops the UTF-8 BOM",
            "tess-orchard",
            vec!["bug", "exports"],
        ),
        (LEDGER, 93) => (
            "Add supplier tags to search",
            "pia-orchard",
            vec!["enhancement"],
        ),
        _ => return None,
    };
    Some(json!({
        "number": number, "title": title, "state": "open", "author": author,
        "body": "Steps to reproduce are in the first comment.", "bodyTruncated": false,
        "labels": labels, "comments": 1, "isPullRequest": false,
        "createdAt": "2026-09-28T10:00:00Z", "updatedAt": "2026-10-06T16:20:00Z",
    }))
}

fn wiki(capability: &str, input: &Value) -> Result<Value, CapabilityCallResult> {
    let title = input.get("title").and_then(Value::as_str).unwrap_or("");
    let dekopon = matches!(title, "Dekopon" | "Shiranui" | "dekopon");
    Ok(match capability {
        "mediawiki.search" => {
            let query = input["query"].as_str().unwrap_or("").to_lowercase();
            let results = if query.contains("dekopon") || query.contains("shiranui") {
                json!([
                    {"page_id": 1_652_211, "title": "Dekopon", "snippet": "Dekopon is a seedless and sweet variety of mandarin orange", "word_count": 912, "modified": "2026-08-14T03:12:00Z"},
                    {"page_id": 51_102_334, "title": "Kiyomi", "snippet": "Kiyomi is a Japanese citrus hybrid", "word_count": 340, "modified": "2026-05-02T11:40:00Z"},
                ])
            } else {
                json!([])
            };
            json!({"results": results, "total_hits": results.as_array().map_or(0, Vec::len), "next_cursor": null, "pagination_capped": false})
        }
        "mediawiki.page" if dekopon => json!({
            "requested_title": title, "title": "Dekopon", "page_id": 1_652_211, "revision_id": 1_243_800_112,
            "description": "Citrus hybrid", "lead": "Dekopon is a seedless and sweet variety of mandarin orange. It is a hybrid between Kiyomi and ponkan. Outside Japan it is sold as Sumo Citrus.",
            "url": "https://en.wikipedia.org/wiki/Dekopon", "wikidata_id": "Q1185094", "is_disambiguation": false,
            "redirects": [], "truncated": false,
        }),
        "mediawiki.outline" if dekopon => json!({
            "title": "Dekopon", "page_id": 1_652_211, "revision_id": 1_243_800_112,
            "sections": [
                {"index": "1", "number": "1", "level": 2, "title": "History", "anchor": "History"},
                {"index": "2", "number": "2", "level": 2, "title": "Cultivation", "anchor": "Cultivation"},
                {"index": "3", "number": "3", "level": 2, "title": "Trademarks", "anchor": "Trademarks"},
            ],
            "truncated": false,
        }),
        "mediawiki.section" if dekopon => {
            let index = input["index"].as_str().unwrap_or("");
            let (heading, text) = match index {
                "1" => (
                    "History",
                    "Dekopon was developed in 1972 at the Kuchinotsu fruit tree research station in Nagasaki, Japan, by crossing Kiyomi and ponkan. Growers in Kumamoto Prefecture took it up in the 1990s.",
                ),
                "2" => (
                    "Cultivation",
                    "The fruit is grown in greenhouses and harvested from December to March.",
                ),
                "3" => (
                    "Trademarks",
                    "Only fruit with a sugar content of at least 13 Brix may be sold as Dekopon.",
                ),
                _ => return Err(not_found()),
            };
            json!({"title": "Dekopon", "page_id": 1_652_211, "revision_id": 1_243_800_112, "index": index, "heading": heading, "text": text, "url": "https://en.wikipedia.org/wiki/Dekopon", "truncated": false})
        }
        "mediawiki.links" if dekopon => {
            json!({"title": "Dekopon", "page_id": 1_652_211, "links": [{"title": "Kiyomi"}, {"title": "Ponkan"}, {"title": "Mandarin orange"}], "next_cursor": null, "pagination_capped": false})
        }
        _ => return Err(not_found()),
    })
}

pub struct Orchard {
    next_asset: AtomicU64,
    attached: parking_lot::Mutex<Vec<u64>>,
}

impl Orchard {
    pub fn new() -> Self {
        Self {
            next_asset: AtomicU64::new(1),
            attached: parking_lot::Mutex::new(Vec::new()),
        }
    }

    fn answer(&self, capability: &str, input: &Value) -> Result<Value, CapabilityCallResult> {
        let repo = input.get("repo").and_then(Value::as_str).unwrap_or("");
        let number = input.get("number").and_then(Value::as_u64).unwrap_or(0);
        let state = input.get("state").and_then(Value::as_str).unwrap_or("open");
        Ok(match capability {
            "gh.pull-request.read" => pull_request(repo, number).ok_or_else(not_found)?,
            "gh.pull-request.list" => {
                if !matches!(repo, LEDGER | PUBLIC) {
                    return Err(not_found());
                }
                let pulls = if state == "closed" {
                    Vec::new()
                } else {
                    pull_numbers(repo)
                        .iter()
                        .filter_map(|number| pull_request(repo, *number))
                        .map(|pull| {
                            json!({"number": pull["number"], "title": pull["title"], "state": "open", "draft": false, "author": pull["author"], "headRef": pull["headRef"], "headSha": pull["headSha"], "baseRef": "main", "createdAt": pull["createdAt"], "updatedAt": pull["updatedAt"]})
                        })
                        .collect()
                };
                json!({"pullRequests": pulls, "page": 1, "hasMore": false})
            }
            "gh.pull-request.files" => {
                pull_request(repo, number).ok_or_else(not_found)?;
                json!({"files": [
                    {"path": "app/jobs/order_export_job.rb", "status": "added", "additions": 88, "deletions": 0, "patchTruncated": false},
                    {"path": "app/services/order_export.rb", "status": "modified", "additions": 64, "deletions": 51, "patchTruncated": false},
                ], "page": 1, "hasMore": false})
            }
            "gh.pull-request.reviews" => {
                pull_request(repo, number).ok_or_else(not_found)?;
                json!({"reviews": [], "page": 1, "hasMore": false})
            }
            "gh.issue.read" => issue(repo, number).ok_or_else(not_found)?,
            "gh.issue.list" => {
                if !matches!(repo, LEDGER | PUBLIC) {
                    return Err(not_found());
                }
                let mut numbers = pull_numbers(repo).to_vec();
                if repo == LEDGER {
                    numbers.extend([93, 91, 88]);
                }
                let issues = if state == "closed" {
                    Vec::new()
                } else {
                    numbers
                        .iter()
                        .filter_map(|number| issue(repo, *number))
                        .map(|issue| {
                            json!({"number": issue["number"], "title": issue["title"], "state": "open", "author": issue["author"], "comments": issue["comments"], "isPullRequest": issue["isPullRequest"], "createdAt": issue["createdAt"], "updatedAt": issue["updatedAt"]})
                        })
                        .collect()
                };
                json!({"issues": issues, "page": 1, "hasMore": false})
            }
            "gh.issue-comments.read" => {
                issue(repo, number).ok_or_else(not_found)?;
                json!({"comments": [{"id": 2_318_000_001_u64, "author": "jun-orchard", "body": "Looks close.", "createdAt": "2026-10-06T12:00:00Z"}], "page": 1, "hasMore": false})
            }
            "gh.issue.comment" => {
                issue(repo, number).ok_or_else(not_found)?;
                json!({"commentId": 2_318_841_907_u64, "issueNumber": number, "author": "orchard-agent[bot]", "createdAt": "2026-10-08T12:00:00Z"})
            }
            "gh.repo.read" => {
                if !matches!(repo, LEDGER | PUBLIC) {
                    return Err(not_found());
                }
                json!({"fullName": repo, "defaultBranch": "main", "visibility": if repo == LEDGER { "private" } else { "public" }, "archived": false})
            }
            "gpt-image.generate" | "gpt-image.edit" => {
                let id = self.next_asset.fetch_add(1, Ordering::SeqCst);
                self.attached.lock().push(id);
                json!({"image": {"generationId": format!("img_{id:04}"), "bytes": 1_843_211, "quality": "medium", "size": "1024x1024", "background": "opaque", "outputFormat": "png"}, "model": "gpt-image-2", "usage": {"inputTokens": 26, "outputTokens": 772, "totalTokens": 798}, "requestId": format!("req_{id:04}")})
            }
            "asset.ls" => {
                let assets = self.attached.lock().iter().map(|id| json!({"id": format!("chat-asset:{id}"), "content_type": "image/png", "encoding": "binary", "bytes": 1_843_211, "seekable": true, "origin": "capability", "sent": false})).collect::<Vec<_>>();
                json!({"assets": assets})
            }
            "asset.send" | "asset.rm" | "asset.cat" => {
                let id = input["id"]
                    .as_str()
                    .and_then(|id| id.parse::<u64>().ok())
                    .unwrap_or(0);
                if !self.attached.lock().contains(&id) {
                    return Err(not_found());
                }
                match capability {
                    "asset.send" => json!({"queued": format!("chat-asset:{id}")}),
                    "asset.rm" => json!({"removed": format!("chat-asset:{id}")}),
                    _ => {
                        return Err(CapabilityCallResult::Failed {
                            error: "invalid-input".to_owned(),
                            detail: Some(ProviderFailureDetail {
                                code: "invalid-input".to_owned(),
                                message: "the asset is not UTF-8 text".to_owned(),
                            }),
                        });
                    }
                }
            }
            _ => return Err(CapabilityCallResult::NotFound),
        })
    }
}

impl CapabilityInvoker for Orchard {
    fn granted(&self) -> Vec<String> {
        GRANTED.iter().map(|id| (*id).to_owned()).collect()
    }

    fn command_words(&self) -> Vec<String> {
        ["asset", "gh", "image", "wikipedia"]
            .map(str::to_owned)
            .to_vec()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("asset".to_owned(), ASSET_HELP.to_owned()),
            ("gh".to_owned(), GH_HELP.to_owned()),
            ("image".to_owned(), IMAGE_HELP.to_owned()),
            ("wikipedia".to_owned(), WIKI_HELP.to_owned()),
        ])
    }

    fn run_command(&self, word: &str, argv: &[String], _stdin_piped: bool) -> Option<CommandRun> {
        let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
        Some(match word {
            "gh" => gh(&argv),
            "wikipedia" => wikipedia(&argv),
            "image" => image(&argv),
            "asset" => asset(&argv),
            _ => return None,
        })
    }

    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        _tree: &TreeContext,
    ) -> CapabilityCallResult {
        let capability = proposal.capability.as_str();
        let answer = if capability.starts_with("mediawiki.") {
            wiki(capability, &proposal.input)
        } else {
            self.answer(capability, &proposal.input)
        };
        match answer {
            Ok(value) => {
                let replied = streams.reply(&value);
                if capability.starts_with("gpt-image.")
                    && matches!(replied, CapabilityCallResult::Succeeded)
                {
                    let id = self.attached.lock().last().copied().unwrap_or(0);
                    CapabilityCallResult::SucceededWithStderr(format!(
                        "[gateway: chat-asset:{id} (image/png, 1843211 stored bytes) attached, not sent]"
                    ))
                } else {
                    replied
                }
            }
            Err(failure) => failure,
        }
    }
}

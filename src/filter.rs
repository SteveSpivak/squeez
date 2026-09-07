use crate::commands::Handler;
use crate::commands::{
    build::BuildHandler, cloud::CloudHandler, data_tool::DataToolHandler,
    database::DatabaseHandler, docker::DockerHandler, fs::FsHandler, generic::GenericHandler,
    git::GitHandler, network::NetworkHandler, next_build::NextBuildHandler,
    package_mgr::PackageMgrHandler, playwright::PlaywrightHandler, runtime::RuntimeHandler,
    test_runner::TestRunnerHandler, text_proc::TextProcHandler, typescript::TypescriptHandler,
    wrangler::WranglerHandler,
};
use crate::config::Config;

pub fn compress(cmd: &str, lines: Vec<String>, config: &Config) -> Vec<String> {
    // E8: a user-declared filter-DSL rule (.squeez/filters.ini or
    // ~/.claude/squeez/filters.ini) for this exact command takes priority
    // over the built-in dispatch table -- that's the whole point of
    // letting a user hand-tune the long tail `discover` surfaces.
    if let Some(def) = crate::filter_dsl::find_for_command(cmd) {
        return crate::filter_dsl::apply(&def, lines);
    }
    let (handler, _name) = detect(cmd);
    handler.compress(cmd, lines, config)
}

/// The dispatch-table handler name for `cmd` (e.g. "git", "generic").
/// Exposed for `discover` (E8) to flag GenericHandler-routed commands as
/// candidates for a custom filter-DSL rule, without needing a downcast on
/// the trait object `detect` otherwise returns.
pub fn handler_name(cmd: &str) -> &'static str {
    detect(cmd).1
}

fn detect(cmd: &str) -> (Box<dyn Handler>, &'static str) {
    // `cd /repo && rg foo` is an rg invocation, not a cd. Dispatch looked only
    // at the first token, so every such command took the generic passthrough:
    // measured over 10,754 real calls, the `cd` bucket was 28.2M of 38.9M input
    // tokens and saved 1.5%, while rg/sed/printf saved 65-85%. Shadowing `cmd`
    // here means the `contains("build")`-style probes below also see the real
    // command, so `cd /path/to/build && cargo x` stops matching on its path.
    let cmd = strip_leading_cd(cmd);
    let name = extract_name(cmd);
    match name.as_str() {
        "git" => (Box::new(GitHandler), "git"),
        "docker" | "docker-compose" | "podman" => (Box::new(DockerHandler), "docker"),
        "npm" | "pnpm" | "yarn" => (Box::new(PackageMgrHandler), "package_mgr"),
        "bun" => {
            // `bun test` / `bun run test` / `bun x vitest` behave like a test runner.
            let rest = cmd.split_whitespace().skip(1);
            if rest.clone().any(|a| a == "test")
                || rest.clone().any(|a| a == "vitest" || a == "jest" || a == "playwright")
            {
                (Box::new(TestRunnerHandler), "test_runner")
            } else {
                (Box::new(PackageMgrHandler), "package_mgr")
            }
        }
        "cargo" => {
            if cmd.split_whitespace().any(|a| a == "test") {
                (Box::new(TestRunnerHandler), "test_runner")
            } else {
                (Box::new(PackageMgrHandler), "package_mgr")
            }
        }
        "jest" | "vitest" | "pytest" | "py.test" | "nextest" => {
            (Box::new(TestRunnerHandler), "test_runner")
        }
        "go" => {
            if cmd.split_whitespace().any(|a| a == "test") {
                (Box::new(TestRunnerHandler), "test_runner")
            } else {
                (Box::new(GenericHandler), "generic")
            }
        }
        "playwright" => (Box::new(PlaywrightHandler), "playwright"),
        "tsc" | "eslint" | "biome" | "ruff" => (Box::new(TypescriptHandler), "typescript"),
        "make" | "cmake" | "gradle" | "mvn" | "xcodebuild" => (Box::new(BuildHandler), "build"),
        "next" => {
            if cmd.contains("build") || cmd.contains("dev") || cmd.contains("start") {
                (Box::new(NextBuildHandler), "next_build")
            } else if cmd.contains("lint") {
                // `next lint` wraps eslint — route to the eslint/tsc handler.
                (Box::new(TypescriptHandler), "typescript")
            } else {
                (Box::new(GenericHandler), "generic")
            }
        }
        "vite" | "turbo" => {
            if cmd.contains("build") {
                (Box::new(BuildHandler), "build")
            } else {
                (Box::new(GenericHandler), "generic")
            }
        }
        "wrangler" => (Box::new(WranglerHandler), "wrangler"),
        "kubectl" | "gh" | "aws" | "gcloud" | "az" => (Box::new(CloudHandler), "cloud"),
        "psql" | "prisma" | "mysql" | "drizzle-kit" => (Box::new(DatabaseHandler), "database"),
        "curl" | "wget" | "http" => (Box::new(NetworkHandler), "network"),
        "node" | "python" | "python3" | "ruby" => (Box::new(RuntimeHandler), "runtime"),
        "find" | "ls" | "du" | "ps" | "env" | "lsof" | "netstat"
        | "cat" | "head" | "tail" | "less" | "more" | "bat"
        | "bfs" => (Box::new(FsHandler), "fs"),
        "ugrep" => (Box::new(TextProcHandler), "text_proc"),
        "monitor" => (Box::new(GenericHandler), "generic"),
        // JSON/YAML/IaC tools
        "jq" | "yq" | "terraform" | "tofu" | "helm" | "pulumi" => {
            (Box::new(DataToolHandler), "data_tool")
        }
        // Text-processing tools: grep match output
        "grep" | "rg" | "awk" | "sed" => (Box::new(TextProcHandler), "text_proc"),
        _ => (Box::new(GenericHandler), "generic"),
    }
}

/// Strip any leading `cd <target>` chain, returning the command that actually runs.
///
/// Separators measured across 7,293 real `cd`-prefixed commands: `&&` 70%,
/// newline 28%, `;` 1%. All three are handled; the remaining 0.4% are shapes
/// like `cd -P /x && y` where the target carries flags, and those fall through
/// unchanged rather than risk mis-dispatching.
///
/// Quote-aware, because the separator search must not fire on a `;` or `&&`
/// inside a quoted path. A bare `cd /path` with no separator is left alone --
/// it genuinely is a cd.
fn strip_leading_cd(cmd: &str) -> &str {
    let mut s = cmd.trim();
    loop {
        let rest = match s.strip_prefix("cd") {
            // require whitespace so `cdk deploy` is never treated as a cd
            Some(r) if r.starts_with(|c: char| c.is_whitespace()) => r,
            _ => return s,
        };
        let Some((_target, tail)) = split_at_separator(rest) else {
            return s;
        };
        let tail = tail.trim_start();
        if tail.is_empty() {
            return s; // `cd /path &&` with nothing after it is not a chain
        }
        s = tail;
    }
}

/// Split at the first unquoted `&&`, `;` or newline. Returns None when the
/// segment holds no separator at all.
fn split_at_separator(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'\\' => i += 1, // escaped char cannot open a quote or a separator
                b'\n' | b';' => return Some((&s[..i], &s[i + 1..])),
                b'&' if bytes.get(i + 1) == Some(&b'&') => {
                    return Some((&s[..i], &s[i + 2..]))
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

fn extract_name(cmd: &str) -> String {
    let wrappers = ["npx ", "bunx ", "pnpm exec ", "yarn exec "];
    let mut s = cmd.trim();
    for part in s.split_whitespace() {
        if part.contains('=') {
            s = s[part.len()..].trim_start();
        } else {
            break;
        }
    }
    for w in &wrappers {
        if s.starts_with(w) {
            s = &s[w.len()..];
        }
    }
    let first = s.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first).to_lowercase()
}

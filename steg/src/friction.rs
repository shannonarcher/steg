//! `report_friction`: the agent files the pain it hits — a tool that
//! misbehaved, a capability it needed and didn't have, a description that
//! misled it — as a GitHub issue on the Steg repo, with the context a
//! maintainer needs attached automatically: versions, the last tool calls
//! that led up to it, optionally the screen of a pane.
//!
//! Issues are filed with the `gh` CLI, which owns authentication (it also
//! honours `GH_TOKEN`), so Steg never stores a credential.  An open issue
//! with the same title gets a comment instead of a duplicate.  If `gh` is
//! missing or fails, the report is saved to a local file and the agent is
//! told where, so nothing is lost.

use anyhow::{anyhow, bail, Context};
use mux::pane::PaneId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Where reports go.
#[derive(Debug, Clone)]
pub struct FrictionConfig {
    /// `owner/repo` on GitHub.
    pub repo: String,
    /// Explicit path to `gh`; `None` means search `PATH`.
    pub gh: Option<PathBuf>,
    /// Directory for reports that could not be filed.
    pub local_dir: PathBuf,
}

impl FrictionConfig {
    /// Defaults: the Steg repo, `gh` from `PATH` (or `$STEG_GH_BIN`), and a
    /// `steg/friction` folder in the platform's local data directory (or
    /// `$STEG_FRICTION_DIR`).
    pub fn defaults(repo: Option<String>) -> Self {
        let repo = repo
            .or_else(|| std::env::var("STEG_FRICTION_REPO").ok())
            .unwrap_or_else(|| DEFAULT_REPO.to_string());
        let gh = std::env::var_os("STEG_GH_BIN").map(PathBuf::from);
        let local_dir = std::env::var_os("STEG_FRICTION_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs_next::data_local_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join("steg")
                    .join("friction")
            });
        Self {
            repo,
            gh,
            local_dir,
        }
    }
}

pub const DEFAULT_REPO: &str = "shannonarcher/steg";

/// What kind of pain it was.  Doubles as the GitHub label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// A tool did the wrong thing or errored when it shouldn't have.
    Bug,
    /// Something you needed to do and had no tool for.
    MissingCapability,
    /// A tool or its description misled you about what it does.
    Confusing,
    /// The instructions or docs were wrong or incomplete.
    Docs,
    #[default]
    Other,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Bug => "bug",
            Category::MissingCapability => "missing-capability",
            Category::Confusing => "confusing",
            Category::Docs => "docs",
            Category::Other => "friction",
        }
    }

    pub const ALL: [Category; 5] = [
        Category::Bug,
        Category::MissingCapability,
        Category::Confusing,
        Category::Docs,
        Category::Other,
    ];
}

/// One tool call, as remembered for the "what led up to this" section.
/// Deliberately thin: tool name, numeric/boolean arguments, outcome.  Never
/// the text the agent typed, which may be a secret.
#[derive(Debug, Clone, Serialize)]
pub struct CallRecord {
    pub tool: String,
    pub args: String,
    pub ok: bool,
    pub ms: u64,
    pub note: String,
}

/// A ring buffer of the most recent tool calls.
#[derive(Debug)]
pub struct RecentCalls {
    inner: Mutex<VecDeque<CallRecord>>,
    capacity: usize,
}

impl RecentCalls {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn push(&self, record: CallRecord) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == self.capacity {
            q.pop_front();
        }
        q.push_back(record);
    }

    pub fn snapshot(&self) -> Vec<CallRecord> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// Summarize tool arguments for the call log: numbers and booleans always,
/// strings only for fields that cannot carry user text.
pub fn summarize_args(args: Option<&serde_json::Map<String, serde_json::Value>>) -> String {
    const SAFE_STRINGS: [&str; 4] = ["key", "mods", "side", "category"];
    let Some(args) = args else {
        return String::new();
    };
    let mut parts: Vec<String> = args
        .iter()
        .filter_map(|(k, v)| match v {
            serde_json::Value::Number(n) => Some(format!("{k}={n}")),
            serde_json::Value::Bool(b) => Some(format!("{k}={b}")),
            serde_json::Value::String(s) if SAFE_STRINGS.contains(&k.as_str()) => {
                Some(format!("{k}={s}"))
            }
            serde_json::Value::String(s) => Some(format!("{k}=<{} chars>", s.chars().count())),
            serde_json::Value::Array(a) => Some(format!("{k}=[{} items]", a.len())),
            _ => None,
        })
        .collect();
    parts.sort();
    parts.join(" ")
}

/// What the agent reports.
#[derive(Debug, Clone)]
pub struct FrictionReport {
    pub title: String,
    pub details: String,
    pub category: Category,
    pub tool: Option<String>,
    pub expected: Option<String>,
    /// A pane's screen at the time, if the agent chose to attach one.
    pub screen: Option<(PaneId, String)>,
    pub recent_calls: Vec<CallRecord>,
}

/// What happened to the report.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FrictionOutcome {
    /// "created": a new issue was opened. "commented": an open issue with
    /// the same title already existed and got your report as a comment.
    /// "saved_locally": the issue could not be filed (see `note`); the
    /// report was written to `path` for the human to file later.
    pub outcome: String,
    pub url: Option<String>,
    pub number: Option<u64>,
    pub path: Option<String>,
    pub note: Option<String>,
}

pub struct FrictionReporter {
    config: FrictionConfig,
    labels_ready: AtomicBool,
}

const GH_TIMEOUT: Duration = Duration::from_secs(45);

impl FrictionReporter {
    pub fn new(config: FrictionConfig) -> Self {
        Self {
            config,
            labels_ready: AtomicBool::new(false),
        }
    }

    pub fn repo(&self) -> &str {
        &self.config.repo
    }

    /// File the report.  Never fails: whatever goes wrong, the report ends
    /// up somewhere and the outcome says where.
    pub async fn file(&self, report: &FrictionReport) -> FrictionOutcome {
        let title = render_title(&report.title);
        let body = self.render_body(report);

        let Some(gh) = self.find_gh() else {
            return self.save_locally(
                &title,
                &body,
                "`gh` (GitHub CLI) was not found on PATH; install it and run `gh auth login`, \
                 or set STEG_GH_BIN",
            );
        };

        match self.file_with_gh(&gh, &title, &body, report.category).await {
            Ok(outcome) => outcome,
            Err(err) => {
                log::warn!("filing friction report with gh failed: {err:#}");
                let why = format!("{err:#}");
                let why = if why.contains("Issues has been disabled") {
                    format!(
                        "GitHub Issues are turned off on {repo} (forks start that way). \
                         Enable them at https://github.com/{repo}/settings under Features",
                        repo = self.config.repo
                    )
                } else if why.contains("HTTP 401")
                    || why.contains("HTTP 403")
                    || why.contains("auth")
                {
                    format!(
                        "gh is not authorized for {}: run `gh auth login`, or set GH_TOKEN ({why})",
                        self.config.repo
                    )
                } else {
                    format!("gh failed: {why}")
                };
                self.save_locally(&title, &body, &why)
            }
        }
    }

    async fn file_with_gh(
        &self,
        gh: &Path,
        title: &str,
        body: &str,
        category: Category,
    ) -> anyhow::Result<FrictionOutcome> {
        self.ensure_labels(gh).await;

        // Dedup is a nicety: if the search fails we still file the issue.
        let existing = match self.find_existing(gh, title).await {
            Ok(existing) => existing,
            Err(err) => {
                log::warn!("could not search for an existing issue: {err:#}");
                None
            }
        };

        if let Some((number, url)) = existing {
            self.api(
                gh,
                "POST",
                &format!("repos/{}/issues/{number}/comments", self.config.repo),
                Some(&serde_json::json!({ "body": body })),
            )
            .await?;
            return Ok(FrictionOutcome {
                outcome: "commented".into(),
                url: Some(url),
                number: Some(number),
                path: None,
                note: Some("an open issue with the same title already existed".into()),
            });
        }

        let mut labels = vec!["friction"];
        if category != Category::Other {
            labels.push(category.label());
        }
        let path = format!("repos/{}/issues", self.config.repo);
        let with_labels = serde_json::json!({ "title": title, "body": body, "labels": labels });
        let created = match self.api(gh, "POST", &path, Some(&with_labels)).await {
            Ok(created) => created,
            Err(err) => {
                // Labels are the usual reason a create fails (no triage
                // permission).  Try once more without them.
                log::warn!("issue create with labels failed ({err:#}); retrying without labels");
                let plain = serde_json::json!({ "title": title, "body": body });
                self.api(gh, "POST", &path, Some(&plain)).await?
            }
        };

        let url = created["html_url"]
            .as_str()
            .ok_or_else(|| anyhow!("issue create response had no html_url: {created}"))?
            .to_string();
        let number = created["number"].as_u64();

        Ok(FrictionOutcome {
            outcome: "created".into(),
            url: Some(url),
            number,
            path: None,
            note: None,
        })
    }

    /// Look for an open `friction` issue whose title matches.
    async fn find_existing(&self, gh: &Path, title: &str) -> anyhow::Result<Option<(u64, String)>> {
        let issues = self
            .api(
                gh,
                "GET",
                &format!(
                    "repos/{}/issues?state=open&labels=friction&per_page=100",
                    self.config.repo
                ),
                None,
            )
            .await?;

        #[derive(Deserialize)]
        struct Issue {
            number: u64,
            title: String,
            html_url: String,
            /// Present when the "issue" is really a pull request.
            pull_request: Option<serde_json::Value>,
        }
        let issues: Vec<Issue> = serde_json::from_value(issues).context("parsing issue list")?;
        let wanted = normalize_title(title);
        Ok(issues
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .find(|i| normalize_title(&i.title) == wanted)
            .map(|i| (i.number, i.html_url)))
    }

    /// Make sure the labels exist.  Best effort, once per process; a 422
    /// for a label that already exists is the expected answer.
    async fn ensure_labels(&self, gh: &Path) {
        if self.labels_ready.swap(true, Ordering::SeqCst) {
            return;
        }
        let labels: [(&str, &str, &str); 5] = [
            (
                "friction",
                "D93F0B",
                "Reported from inside Steg by an agent via report_friction",
            ),
            ("bug", "D73A4A", "Something isn't working"),
            (
                "missing-capability",
                "0075CA",
                "An agent needed to do something Steg has no tool for",
            ),
            (
                "confusing",
                "E4E669",
                "A tool or description misled an agent",
            ),
            (
                "docs",
                "0052CC",
                "Instructions or docs are wrong or incomplete",
            ),
        ];
        for (name, color, description) in labels {
            if let Err(err) = self
                .api(
                    gh,
                    "POST",
                    &format!("repos/{}/labels", self.config.repo),
                    Some(&serde_json::json!({
                        "name": name,
                        "color": color,
                        "description": description,
                    })),
                )
                .await
            {
                log::debug!("label {name}: {err:#}");
            }
        }
    }

    /// Call the GitHub REST API through `gh api`, which supplies the
    /// credentials.  REST rather than `gh issue ...` because those
    /// subcommands need GraphQL, which app tokens and some proxies lack.
    async fn api(
        &self,
        gh: &Path,
        method: &str,
        path: &str,
        input: Option<&serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let mut args: Vec<String> =
            vec!["api".into(), "--method".into(), method.into(), path.into()];
        let input_file = match input {
            Some(value) => {
                let f = write_temp(&serde_json::to_string(value)?)?;
                args.push("--input".into());
                args.push(f.path().to_str().context("temp path")?.to_string());
                Some(f)
            }
            None => None,
        };
        let out = self.gh(gh, &args).await?;
        drop(input_file);
        if out.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(out.trim())
            .with_context(|| format!("parsing gh api {path} response: {out:?}"))
    }

    /// Run `gh` with arguments, returning stdout; stderr goes into the error.
    async fn gh<S: AsRef<OsStr>>(&self, gh: &Path, args: &[S]) -> anyhow::Result<String> {
        let mut cmd = tokio::process::Command::new(gh);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Never let gh try to open a pager or a browser.
            .env("GH_PAGER", "cat")
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");
        let output = tokio::time::timeout(GH_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow!("gh timed out after {GH_TIMEOUT:?}"))?
            .with_context(|| format!("running {}", gh.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Show the call without the temp-file argument, which is noise.
            let shown: Vec<String> = args
                .iter()
                .map(|a| a.as_ref().to_string_lossy().into_owned())
                .take_while(|a| a != "--input")
                .collect();
            bail!(
                "`gh {}` exited with {}: {}{}",
                shown.join(" "),
                output.status,
                stderr.trim(),
                if stdout.trim().is_empty() || stderr.contains(stdout.trim()) {
                    String::new()
                } else {
                    format!(" (stdout: {})", stdout.trim())
                }
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn find_gh(&self) -> Option<PathBuf> {
        if let Some(explicit) = &self.config.gh {
            return explicit.is_file().then(|| explicit.clone());
        }
        let path = std::env::var_os("PATH")?;
        let names: &[&str] = if cfg!(windows) {
            &["gh.exe", "gh.cmd", "gh"]
        } else {
            &["gh"]
        };
        std::env::split_paths(&path)
            .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
            .find(|candidate| candidate.is_file())
    }

    fn save_locally(&self, title: &str, body: &str, why: &str) -> FrictionOutcome {
        let result = (|| -> anyhow::Result<PathBuf> {
            std::fs::create_dir_all(&self.config.local_dir)
                .with_context(|| format!("creating {}", self.config.local_dir.display()))?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let path = self
                .config
                .local_dir
                .join(format!("{stamp}-{}.md", slug(title)));
            std::fs::write(
                &path,
                format!(
                    "# {title}\n\n_Not filed to {repo}: {why}_\n\n{body}",
                    repo = self.config.repo
                ),
            )
            .with_context(|| format!("writing {}", path.display()))?;
            Ok(path)
        })();

        match result {
            Ok(path) => FrictionOutcome {
                outcome: "saved_locally".into(),
                url: None,
                number: None,
                path: Some(path.display().to_string()),
                note: Some(format!(
                    "{why}. Saved the report locally; ask the human to file it to {}.",
                    self.config.repo
                )),
            },
            Err(err) => FrictionOutcome {
                outcome: "lost".into(),
                url: None,
                number: None,
                path: None,
                note: Some(format!(
                    "{why}, and saving locally also failed: {err:#}. \
                     Tell the human about this directly."
                )),
            },
        }
    }

    pub fn render_body(&self, report: &FrictionReport) -> String {
        let mut body = String::new();
        body.push_str(
            "_Reported from inside Steg by an agent, via the `report_friction` tool._\n\n",
        );
        body.push_str("## What happened\n\n");
        body.push_str(report.details.trim());
        body.push_str("\n\n");

        if let Some(expected) = report
            .expected
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            body.push_str("## Expected\n\n");
            body.push_str(expected);
            body.push_str("\n\n");
        }

        body.push_str("## Context\n\n");
        body.push_str(&format!("- Category: `{}`\n", report.category.label()));
        if let Some(tool) = &report.tool {
            body.push_str(&format!("- Tool: `{tool}`\n"));
        }
        body.push_str(&format!(
            "- Steg {} (wezterm {})\n- OS: {} {}\n\n",
            env!("CARGO_PKG_VERSION"),
            config::wezterm_version(),
            std::env::consts::OS,
            std::env::consts::ARCH,
        ));

        if !report.recent_calls.is_empty() {
            body.push_str("## Recent tool calls\n\n");
            body.push_str("Oldest first; `text` arguments are shown as lengths only.\n\n");
            body.push_str("| tool | arguments | ok | ms | note |\n|---|---|---|---|---|\n");
            for c in &report.recent_calls {
                body.push_str(&format!(
                    "| `{}` | {} | {} | {} | {} |\n",
                    c.tool,
                    md_cell(&c.args),
                    if c.ok { "yes" } else { "**no**" },
                    c.ms,
                    md_cell(&c.note),
                ));
            }
            body.push('\n');
        }

        if let Some((pane_id, screen)) = &report.screen {
            body.push_str(&format!(
                "<details><summary>Screen of pane {pane_id} when reported</summary>\n\n```text\n{}\n```\n\n</details>\n",
                truncate_lines(screen, 200)
            ));
        }

        body
    }
}

fn render_title(title: &str) -> String {
    let t: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim_end_matches(['.', '!']);
    if t.chars().count() > 120 {
        let cut: String = t.chars().take(117).collect();
        format!("{cut}...")
    } else if t.is_empty() {
        "Untitled friction report".to_string()
    } else {
        t.to_string()
    }
}

fn normalize_title(title: &str) -> String {
    title
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '!', '?'])
        .to_string()
}

fn slug(title: &str) -> String {
    let s: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    s.chars().take(60).collect()
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn truncate_lines(s: &str, max: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max {
        return s.to_string();
    }
    let keep = &lines[lines.len() - max..];
    format!(
        "... ({} lines above omitted)\n{}",
        lines.len() - max,
        keep.join("\n")
    )
}

fn write_temp(body: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut f = tempfile::Builder::new()
        .prefix("steg-friction-")
        .suffix(".md")
        .tempfile()
        .context("creating temp file for issue body")?;
    std::io::Write::write_all(&mut f, body.as_bytes()).context("writing issue body")?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_tidied_and_matched_loosely() {
        assert_eq!(
            render_title("  wait_for_idle   returns early. "),
            "wait_for_idle returns early"
        );
        assert_eq!(render_title(""), "Untitled friction report");
        assert_eq!(render_title(&"x".repeat(200)).chars().count(), 120);
        assert_eq!(
            normalize_title("Wait_For_Idle returns  early!"),
            normalize_title("wait_for_idle returns early")
        );
    }

    #[test]
    fn args_summary_never_leaks_typed_text() {
        let args = serde_json::json!({
            "pane_id": 7,
            "text": "hunter2",
            "enter": true,
            "key": "c",
            "mods": "CTRL",
            "command": ["bash", "-l"],
            "cwd": "/home/me"
        });
        let s = summarize_args(args.as_object());
        assert!(!s.contains("hunter2"), "{s}");
        assert!(!s.contains("/home/me"), "{s}");
        assert!(s.contains("pane_id=7") && s.contains("enter=true"), "{s}");
        assert!(s.contains("key=c") && s.contains("mods=CTRL"), "{s}");
        assert!(
            s.contains("text=<7 chars>") && s.contains("command=[2 items]"),
            "{s}"
        );
    }

    #[test]
    fn recent_calls_is_a_ring() {
        let r = RecentCalls::new(2);
        for i in 0..3 {
            r.push(CallRecord {
                tool: format!("t{i}"),
                args: String::new(),
                ok: true,
                ms: 0,
                note: String::new(),
            });
        }
        let names: Vec<_> = r.snapshot().into_iter().map(|c| c.tool).collect();
        assert_eq!(names, vec!["t1", "t2"]);
    }

    #[test]
    fn body_has_the_sections_a_maintainer_needs() {
        let reporter = FrictionReporter::new(FrictionConfig {
            repo: "example/steg".into(),
            gh: None,
            local_dir: std::env::temp_dir(),
        });
        let body = reporter.render_body(&FrictionReport {
            title: "t".into(),
            details: "It broke | badly".into(),
            category: Category::Bug,
            tool: Some("send_key".into()),
            expected: Some("Not broken".into()),
            screen: Some((3, "line1\nline2".into())),
            recent_calls: vec![CallRecord {
                tool: "send_key".into(),
                args: "pane_id=3".into(),
                ok: false,
                ms: 12,
                note: "unknown key".into(),
            }],
        });
        for needle in [
            "## What happened",
            "It broke | badly",
            "## Expected",
            "Not broken",
            "Category: `bug`",
            "Tool: `send_key`",
            "## Recent tool calls",
            "| `send_key` | pane_id=3 | **no** | 12 | unknown key |",
            "Screen of pane 3",
            "line1\nline2",
            std::env::consts::OS,
        ] {
            assert!(body.contains(needle), "missing {needle:?} in:\n{body}");
        }
    }

    #[test]
    fn long_screens_keep_the_bottom() {
        let screen: String = (1..=300).map(|i| format!("row{i}\n")).collect();
        let t = truncate_lines(&screen, 200);
        assert!(t.starts_with("... (100 lines above omitted)"));
        assert!(t.contains("row300"));
        assert!(!t.contains("row100\n"));
    }
}

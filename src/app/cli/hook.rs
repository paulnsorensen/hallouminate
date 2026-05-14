use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

const BLOCK_START: &str = "# hallouminate-managed-block";
const BLOCK_END: &str = "# /hallouminate-managed-block";
const HOOK_BODY: &str = r#"hallouminate index >/dev/null 2>&1 &"#;
const SHEBANG: &str = "#!/bin/sh";
const HOOK_FILES: &[&str] = &["post-commit", "post-merge"];

#[derive(Debug, Default, Clone)]
pub struct HookArgs {
    pub repo: Option<PathBuf>,
}

pub fn cmd_hook_install(args: HookArgs) -> anyhow::Result<()> {
    let hooks_dir = resolve_hooks_dir(args.repo.as_deref())?;
    fs::create_dir_all(&hooks_dir).with_context(|| format!("create {}", hooks_dir.display()))?;
    for name in HOOK_FILES {
        install_managed_block(&hooks_dir.join(name))?;
    }
    println!("installed hallouminate hooks in {}", hooks_dir.display());
    Ok(())
}

pub fn cmd_hook_uninstall(args: HookArgs) -> anyhow::Result<()> {
    let hooks_dir = resolve_hooks_dir(args.repo.as_deref())?;
    for name in HOOK_FILES {
        uninstall_managed_block(&hooks_dir.join(name))?;
    }
    println!("removed hallouminate hooks from {}", hooks_dir.display());
    Ok(())
}

fn resolve_hooks_dir(repo: Option<&Path>) -> anyhow::Result<PathBuf> {
    let root = match repo {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("resolve current dir")?,
    };
    // Defer to git itself so worktrees (where `.git` is a *file* pointing at
    // `<main>/.git/worktrees/<name>`), submodules, and `core.hooksPath` all
    // resolve to whatever git would actually invoke at hook time. The naive
    // `<root>/.git/hooks` assumption breaks in worktrees — git puts hooks at
    // the common gitdir, not the per-worktree gitdir.
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .with_context(|| format!("invoke `git -C {} rev-parse`", root.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "not a git repository: {} ({})",
            root.display(),
            stderr.trim()
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .context("git rev-parse output not UTF-8")?
        .trim();
    let resolved = PathBuf::from(stdout);
    Ok(if resolved.is_absolute() {
        resolved
    } else {
        root.join(resolved)
    })
}

fn install_managed_block(hook_path: &Path) -> anyhow::Result<()> {
    let existing = read_hook_or_empty(hook_path)?;
    let stripped = strip_managed_block(&existing)
        .with_context(|| format!("hook {}", hook_path.display()))?;
    let mut next = ensure_shebang(&stripped);
    if !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(BLOCK_START);
    next.push('\n');
    next.push_str(HOOK_BODY);
    next.push('\n');
    next.push_str(BLOCK_END);
    next.push('\n');
    write_executable(hook_path, &next)
}

fn uninstall_managed_block(hook_path: &Path) -> anyhow::Result<()> {
    if !hook_path.exists() {
        return Ok(());
    }
    let existing =
        fs::read_to_string(hook_path).with_context(|| format!("read {}", hook_path.display()))?;
    let stripped = strip_managed_block(&existing)
        .with_context(|| format!("hook {}", hook_path.display()))?;
    if is_only_shebang_or_empty(&stripped) {
        fs::remove_file(hook_path).with_context(|| format!("remove {}", hook_path.display()))?;
    } else {
        write_executable(hook_path, &stripped)?;
    }
    Ok(())
}

fn read_hook_or_empty(path: &Path) -> anyhow::Result<String> {
    if path.exists() {
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
    } else {
        Ok(String::new())
    }
}

fn strip_managed_block(text: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut in_block = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == BLOCK_START {
            in_block = true;
            continue;
        }
        if in_block {
            if trimmed == BLOCK_END {
                in_block = false;
            }
            continue;
        }
        out.push_str(line);
    }
    if in_block {
        // BLOCK_START without a matching BLOCK_END means the file was
        // truncated, hand-edited, or otherwise corrupted. Rewriting would
        // silently drop every line after the orphan marker (the user's
        // content), so refuse loudly and let them resolve it.
        return Err(anyhow!(
            "malformed hook: `{BLOCK_START}` is not closed by `{BLOCK_END}`; \
             refusing to rewrite to avoid losing content after the orphan marker"
        ));
    }
    Ok(out)
}

fn ensure_shebang(text: &str) -> String {
    if text.is_empty() {
        return format!("{SHEBANG}\n");
    }
    if text.starts_with("#!") {
        return text.to_string();
    }
    format!("{SHEBANG}\n{text}")
}

fn is_only_shebang_or_empty(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty() || trimmed == SHEBANG
}

#[cfg(unix)]
fn write_executable(path: &Path, content: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, content).with_context(|| format!("write {}", path.display()))?;
    let mut perms = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_executable(path: &Path, content: &str) -> anyhow::Result<()> {
    fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        // Real `git init` so `git rev-parse --git-path hooks` resolves
        // correctly. The hooks dir git creates includes `.sample` files;
        // our installer writes `post-commit` and `post-merge` alongside
        // them, so the existing assertions still hold.
        git_in(dir, &["init", "--quiet"]);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("invoke git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn count_managed_blocks(text: &str) -> usize {
        text.match_indices(BLOCK_START).count()
    }

    #[test]
    fn install_in_fresh_repo_writes_both_hook_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        cmd_hook_install(HookArgs {
            repo: Some(dir.path().into()),
        })
        .expect("install");
        for name in HOOK_FILES {
            let body = fs::read_to_string(dir.path().join(".git/hooks").join(name))
                .unwrap_or_else(|_| panic!("read {name}"));
            assert!(body.starts_with("#!/bin/sh"), "{name} missing shebang");
            assert_eq!(count_managed_blocks(&body), 1, "{name} block count");
            assert!(body.contains(HOOK_BODY), "{name} missing hook body");
        }
    }

    #[test]
    fn install_twice_leaves_single_managed_block_per_hook() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        let args = HookArgs {
            repo: Some(dir.path().into()),
        };
        cmd_hook_install(args.clone()).expect("first install");
        cmd_hook_install(args).expect("second install");
        for name in HOOK_FILES {
            let body = fs::read_to_string(dir.path().join(".git/hooks").join(name))
                .unwrap_or_else(|_| panic!("read {name}"));
            assert_eq!(
                count_managed_blocks(&body),
                1,
                "{name} must contain one block"
            );
        }
    }

    #[test]
    fn uninstall_removes_managed_block_and_drops_empty_hook_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        let args = HookArgs {
            repo: Some(dir.path().into()),
        };
        cmd_hook_install(args.clone()).expect("install");
        cmd_hook_uninstall(args).expect("uninstall");
        for name in HOOK_FILES {
            let path = dir.path().join(".git/hooks").join(name);
            assert!(
                !path.exists(),
                "{name} file should be removed when only managed block remained"
            );
        }
    }

    #[test]
    fn uninstall_preserves_user_lines_outside_managed_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        let post_commit = dir.path().join(".git/hooks/post-commit");
        fs::write(&post_commit, "#!/bin/sh\necho 'user line'\n").expect("seed");
        let args = HookArgs {
            repo: Some(dir.path().into()),
        };
        cmd_hook_install(args.clone()).expect("install");
        cmd_hook_uninstall(args).expect("uninstall");
        let body = fs::read_to_string(&post_commit).expect("read post-commit");
        assert!(body.contains("echo 'user line'"), "user line lost: {body}");
        assert_eq!(count_managed_blocks(&body), 0, "managed block remains");
    }

    #[test]
    fn install_appends_block_to_existing_hook_with_user_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        let post_merge = dir.path().join(".git/hooks/post-merge");
        fs::write(&post_merge, "#!/bin/sh\necho 'pre-existing'\n").expect("seed");
        cmd_hook_install(HookArgs {
            repo: Some(dir.path().into()),
        })
        .expect("install");
        let body = fs::read_to_string(&post_merge).expect("read post-merge");
        assert!(body.contains("echo 'pre-existing'"), "user line lost");
        assert_eq!(count_managed_blocks(&body), 1, "exactly one block");
        assert!(body.contains(HOOK_BODY), "missing hook body");
    }

    #[test]
    fn install_errors_when_target_is_not_a_git_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = cmd_hook_install(HookArgs {
            repo: Some(dir.path().into()),
        })
        .expect_err("not a git repo");
        assert!(err.to_string().contains("not a git repository"), "{err}");
    }

    #[test]
    fn install_in_worktree_writes_to_main_repo_hooks() {
        // Regression for PR #8 age review: git worktrees use a `.git` file
        // (containing `gitdir: <main>/.git/worktrees/<name>`) instead of a
        // `.git` directory. The previous `<root>/.git/hooks` assumption
        // broke in worktrees; `git rev-parse --git-path hooks` resolves to
        // the main repo's `.git/hooks/` so install must land there.
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("main");
        let wt = dir.path().join("wt");
        fs::create_dir_all(&main).expect("mkdir main");
        init_repo(&main);
        // `git worktree add` needs a starting commit; give it an empty one
        // with an inline identity (CI runners don't have user.email set).
        git_in(
            &main,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        git_in(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                wt.to_str().expect("utf8 path"),
            ],
        );
        // Sanity: confirm the worktree really does have a `.git` *file*,
        // not a directory, so the test exercises the bug it's defending.
        let wt_git = wt.join(".git");
        assert!(wt_git.is_file(), "worktree .git must be a file");

        cmd_hook_install(HookArgs {
            repo: Some(wt.clone()),
        })
        .expect("install from worktree");

        for name in HOOK_FILES {
            let in_main = main.join(".git/hooks").join(name);
            assert!(
                in_main.exists(),
                "hook {name} must be installed at the main repo's .git/hooks; \
                 missing: {}",
                in_main.display()
            );
            let body = fs::read_to_string(&in_main).expect("read main hook");
            assert_eq!(
                count_managed_blocks(&body),
                1,
                "{name} must contain one managed block"
            );
        }
    }

    #[test]
    fn uninstall_is_noop_when_no_hooks_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        cmd_hook_uninstall(HookArgs {
            repo: Some(dir.path().into()),
        })
        .expect("uninstall noop");
    }

    #[test]
    fn strip_managed_block_errors_when_block_is_unterminated() {
        // Regression for PR #8 Copilot review: an orphan BLOCK_START with no
        // matching BLOCK_END must NOT silently drop the user's lines that
        // follow it. The function refuses to rewrite and surfaces the error.
        let text = format!("#!/bin/sh\n{BLOCK_START}\nold body\necho 'kept content'\n");
        let err = strip_managed_block(&text).expect_err("unterminated block must error");
        let msg = err.to_string();
        assert!(msg.contains(BLOCK_START), "missing BLOCK_START in: {msg}");
        assert!(msg.contains(BLOCK_END), "missing BLOCK_END in: {msg}");
    }

    #[test]
    fn install_refuses_when_existing_hook_has_unterminated_managed_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(dir.path());
        let hooks = dir.path().join(".git/hooks");
        fs::create_dir_all(&hooks).expect("mkdir hooks");
        let post_commit = hooks.join("post-commit");
        let seeded = format!(
            "#!/bin/sh\n{BLOCK_START}\nlegacy body\necho 'must-not-be-dropped'\n"
        );
        fs::write(&post_commit, &seeded).expect("seed");

        let err = cmd_hook_install(HookArgs {
            repo: Some(dir.path().into()),
        })
        .expect_err("install must refuse to rewrite a malformed hook");
        let chain = format!("{err:#}");
        assert!(chain.contains("malformed hook"), "{chain}");

        // The original file must be left untouched so the user can see and
        // resolve the corruption rather than discover lost content later.
        let after = fs::read_to_string(&post_commit).expect("read post-commit");
        assert_eq!(after, seeded, "hook file must be untouched on refusal");
    }
}

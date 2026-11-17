//! 把音效送进 DaVinci Resolve 时间线的播放头位置。
//!
//! Resolve 官方只开放 Python 和 Lua 两种脚本入口，`fusionscript` 导出的是
//! PyObject，Rust 侧无法直接绑定。所以这里把一段 Python 脚本写到临时目录，
//! 交给系统 Python 执行；脚本内部通过 DaVinciResolveScript 连接正在运行的
//! Resolve，把文件导入媒体池并按 recordFrame 放到播放头。
//!
//! 返回的字符串直接进状态栏，因此成功和失败都写成给人看的中文。

use std::path::Path;
use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
use std::path::PathBuf;

/// 内嵌的桥接脚本，编译进二进制，运行时不依赖外部文件。
const BRIDGE_SCRIPT: &str = include_str!("resolve_bridge.py");

/// `CreateProcess` 的 CREATE_NO_WINDOW。桌面程序没有控制台，启动 python.exe
/// 时如果不带这个标志，Windows 会为子进程新开一个黑框再关掉。
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 把 `path` 指向的音频插入 Resolve 当前时间线的播放头。
///
/// 成功返回一句可直接展示的结果，失败返回一句可直接展示的原因。
pub fn import_to_timeline(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err(format!("找不到音频文件：{}", path.display()));
    }

    let python = find_python().ok_or_else(|| {
        "没有找到 Python 3。达芬奇的脚本接口需要 Python 3.8 以上：\
         Windows 请从 python.org 安装（勾选 Add Python to PATH），\
         macOS 可运行 xcode-select --install。"
            .to_string()
    })?;

    // 临时脚本名带上进程号，多个实例同时点也不会互相覆盖。
    let script_path =
        std::env::temp_dir().join(format!("audiolib_resolve_bridge_{}.py", std::process::id()));
    std::fs::write(&script_path, BRIDGE_SCRIPT)
        .map_err(|error| format!("写入桥接脚本失败：{error}"))?;

    let mut command = Command::new(&python[0]);
    command.args(&python[1..]).arg(&script_path).arg(path);
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);

    let output = command.output();

    // 临时脚本用完即删，失败也不留垃圾。
    let _ = std::fs::remove_file(&script_path);

    let output = output.map_err(|error| format!("启动 Python 失败：{error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if output.status.success() {
        if stdout.is_empty() {
            return Ok("已送进达芬奇时间线".to_string());
        }
        return Ok(stdout);
    }

    if !stdout.is_empty() {
        return Err(stdout);
    }
    if !stderr.is_empty() {
        // Python 自己的报错栈很长，只留最后一行有意义的信息。
        let tail = stderr.lines().last().unwrap_or(&stderr).trim().to_string();
        return Err(format!("达芬奇桥接失败：{tail}"));
    }
    Err("达芬奇桥接失败，没有返回任何信息".to_string())
}

/// 找到可用的 Python 3 解释器，返回完整的命令前缀。
///
/// 第一个元素是程序路径，其余是前置参数（`py -3` 这种启动器需要）。
/// 找到第一个能跑通 `--version` 的就算数。
fn find_python() -> Option<Vec<String>> {
    for candidate in python_candidates() {
        let Some((program, args)) = candidate.split_first() else {
            continue;
        };
        let mut probe = Command::new(program);
        probe.args(args).arg("--version");
        #[cfg(target_os = "windows")]
        probe.creation_flags(CREATE_NO_WINDOW);
        let usable = probe
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if usable {
            return Some(candidate);
        }
    }
    None
}

/// 按优先级列出候选解释器。PATH 里的名字排前面，装到默认目录但没进 PATH
/// 的排后面，尽量让用户不用手动配环境。
fn python_candidates() -> Vec<Vec<String>> {
    let mut candidates: Vec<Vec<String>> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        for path in [
            "/usr/bin/python3",
            "/usr/local/bin/python3",
            "/opt/homebrew/bin/python3",
        ] {
            candidates.push(vec![path.to_string()]);
        }
    }

    #[cfg(target_os = "windows")]
    {
        // py 启动器要显式要 3.x，否则可能落到 Python 2 上。
        candidates.push(vec!["python".to_string()]);
        candidates.push(vec!["python3".to_string()]);
        candidates.push(vec!["py".to_string(), "-3".to_string()]);
        for path in windows_python_installations() {
            candidates.push(vec![path]);
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        candidates.push(vec!["python3".to_string()]);
        candidates.push(vec!["python".to_string()]);
    }

    candidates
}

/// 扫 `%LOCALAPPDATA%\Programs\Python` 和 `%ProgramFiles%\Python` 下的
/// `Python3xx` 目录，返回 `python.exe` 的绝对路径。新版本排前面。
#[cfg(target_os = "windows")]
fn windows_python_installations() -> Vec<String> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Programs").join("Python"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("Python"));
    }

    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut directories: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        // 目录名形如 Python313，字典序倒排即可让高版本优先。
        directories.sort();
        directories.reverse();
        for directory in directories {
            let executable = directory.join("python.exe");
            if executable.is_file() {
                found.push(executable);
            }
        }
    }

    found
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

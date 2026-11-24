#!/usr/bin/env python3
"""把单个音频文件导入 DaVinci Resolve 当前时间线的播放头位置。

由 AudioLib Explorer 的 Rust 侧以子进程方式调用：
    python3 audiolib_resolve_bridge.py <音频文件绝对路径>

成功时往 stdout 打一行中文结果，失败时打一行中文原因并以非零码退出。
Resolve 只提供 Python / Lua 脚本入口，这是 Rust 侧唯一可靠的接入方式。
"""

import os
import platform
import subprocess
import sys


CREATE_NO_WINDOW = 0x08000000


def windows_resolve_script_api():
    """找 DaVinci Resolve 的脚本 API 目录（Support/Developer/Scripting）。

    ProgramData 一般固定在系统盘，但装在别的盘或改过系统目录时也要能找到，
    所以先按环境变量找，再逐盘兜底。
    """
    candidates = []
    program_data = os.environ.get("PROGRAMDATA")
    if program_data:
        candidates.append(program_data)
    candidates.append(r"C:\ProgramData")
    for letter in "CDEFGHIJKLMNOPQRSTUVWXYZ":
        candidates.append(letter + ":\\ProgramData")

    seen = set()
    for root in candidates:
        path = os.path.join(root, "Blackmagic Design", "DaVinci Resolve",
                            "Support", "Developer", "Scripting")
        key = os.path.normcase(path)
        if key in seen:
            continue
        seen.add(key)
        if os.path.isdir(path):
            return path
    return None


def windows_resolve_script_library():
    """找 fusionscript.dll。

    Resolve 可以装在任意盘任意目录（很多人装在 D:/E: 盘的自定义路径），
    光看 %PROGRAMFILES% 会漏。这里按可靠性从高到低试：
      1. 用户自己配的 RESOLVE_SCRIPT_LIB
      2. 正在运行的 Resolve.exe 所在目录（最准）
      3. PROGRAMFILES 系列环境变量
      4. 所有盘符下的 Program Files / Program Files (x86)
    """
    candidates = []

    configured = os.environ.get("RESOLVE_SCRIPT_LIB")
    if configured:
        candidates.append(configured)

    # 从运行中的 Resolve 进程拿安装目录。导入本来也要求 Resolve 开着，
    # 所以这一步几乎总能命中，且不受自定义安装路径影响。
    try:
        completed = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command",
             "(Get-Process Resolve -ErrorAction SilentlyContinue | "
             "Select-Object -First 1).Path"],
            capture_output=True, text=True, timeout=20,
            creationflags=CREATE_NO_WINDOW,
        )
        executable = (completed.stdout or "").strip()
        if executable and os.path.isfile(executable):
            candidates.append(
                os.path.join(os.path.dirname(executable), "fusionscript.dll"))
    except Exception:  # noqa: BLE001 - 探测失败就走后面的兜底
        pass

    roots = []
    for variable in ("PROGRAMFILES", "PROGRAMW6432", "PROGRAMFILES(X86)"):
        value = os.environ.get(variable)
        if value:
            roots.append(value)
    for letter in "CDEFGHIJKLMNOPQRSTUVWXYZ":
        for sub in ("Program Files", "Program Files (x86)"):
            roots.append(letter + ":\\" + sub)
    for root in roots:
        candidates.append(os.path.join(root, "Blackmagic Design",
                                       "DaVinci Resolve", "fusionscript.dll"))

    seen = set()
    for candidate in candidates:
        key = os.path.normcase(candidate)
        if key in seen:
            continue
        seen.add(key)
        if os.path.isfile(candidate):
            return candidate
    return None


def configure_resolve_environment():
    """设置 Resolve 脚本 API 需要的环境变量，返回错误说明或 None。"""
    system = platform.system()
    if system == "Darwin":
        api = ("/Library/Application Support/Blackmagic Design/"
               "DaVinci Resolve/Developer/Scripting")
        library = ("/Applications/DaVinci Resolve/DaVinci Resolve.app/"
                   "Contents/Libraries/Fusion/fusionscript.so")
    elif system == "Windows":
        api = windows_resolve_script_api()
        if not api:
            return ("找不到达芬奇脚本目录：各盘 ProgramData\\Blackmagic Design"
                    "\\DaVinci Resolve\\Support\\Developer\\Scripting 都不存在。"
                    "请确认装的是 DaVinci Resolve Studio 且安装完整。")
        library = windows_resolve_script_library()
        if not library:
            return ("找不到 fusionscript.dll：已检查 %PROGRAMFILES%、"
                    "正在运行的 Resolve 安装目录，以及各盘 Program Files 下的 "
                    "Blackmagic Design\\DaVinci Resolve。请确认 Resolve 正在运行，"
                    "或手动设置环境变量 RESOLVE_SCRIPT_LIB 指向 fusionscript.dll。")
    else:
        return "暂不支持当前系统，达芬奇桥接目前只覆盖 macOS 和 Windows"

    if not os.path.isdir(api):
        return "找不到达芬奇脚本目录：" + api
    if not os.path.isfile(library):
        return "找不到达芬奇脚本库：" + library

    os.environ["RESOLVE_SCRIPT_API"] = api
    os.environ["RESOLVE_SCRIPT_LIB"] = library

    modules = os.path.join(api, "Modules")
    if os.path.isdir(modules):
        sys.path.insert(0, modules)
        existing = os.environ.get("PYTHONPATH", "")
        os.environ["PYTHONPATH"] = (
            modules + (os.pathsep + existing if existing else "")
        )
    return None


class BridgeError(Exception):
    """能直接展示给用户的桥接错误。"""


def api_call(obj, name, *args):
    """调用达芬奇脚本 API，并在方法缺失时给出可读提示。

    老版本达芬奇的 Python 绑定里，不存在的方法属性会是 None，
    直接调用只会抛 `TypeError: 'NoneType' object is not callable`，
    看不出是哪一步；这里统一检查，把方法名带出来。
    """
    method = getattr(obj, name, None)
    if not callable(method):
        raise BridgeError(
            "当前达芬奇版本不支持 %s()，请升级到 DaVinci Resolve 18.6 或更新版本" % name
        )
    return method(*args)


def as_int(value, fallback):
    """Resolve 的属性一律以字符串返回，这里统一转成整数。"""
    if value is None or value == "":
        return fallback
    try:
        return int(str(value).strip())
    except (TypeError, ValueError):
        return fallback


def frame_from_timecode(timecode, fps):
    """把 HH:MM:SS:FF 换算成帧号，兼容 29.97 / 59.94 的丢帧记法。"""
    if not timecode:
        return 0
    drop_frame = ";" in timecode
    parts = timecode.replace(";", ":").split(":")
    if len(parts) != 4:
        return 0
    try:
        hours, minutes, seconds, frames = (int(part) for part in parts)
    except ValueError:
        return 0

    nominal = int(round(fps)) or 24
    if not drop_frame:
        return (hours * 3600 + minutes * 60 + seconds) * nominal + frames

    drop_per_minute = 2 if nominal == 30 else 4
    total_minutes = hours * 60 + minutes
    return (
        (hours * 3600 + minutes * 60 + seconds) * nominal
        + frames
        - drop_per_minute * (total_minutes - total_minutes // 10)
    )


def normalized(path):
    """跨平台比较路径用：统一分隔符、忽略大小写。"""
    return os.path.normcase(os.path.normpath(path or ""))


def find_existing_clip(media_pool, audio_path):
    """已经在媒体池里的文件再 ImportMedia 会返回空，这里按路径找回已有的 clip。"""
    root = media_pool.GetRootFolder()
    if not root:
        return None
    target = normalized(audio_path)
    pending = [root]
    while pending:
        folder = pending.pop()
        try:
            clips = folder.GetClipList() or []
            pending.extend(folder.GetSubFolderList() or [])
        except Exception:  # noqa: BLE001 - 单个坏节点不该中断整个查找
            continue
        for clip in clips:
            try:
                path = (clip.GetClipProperty() or {}).get("File Path") or ""
            except Exception:  # noqa: BLE001
                continue
            if path and normalized(path) == target:
                return clip
    return None


def clip_span(item, fps):
    """返回素材自身的入点与出点帧号。Resolve 给的是字符串，要逐级兜底。"""
    properties = api_call(item, "GetClipProperty") or {}
    start = as_int(properties.get("Start"), 0)

    end = as_int(properties.get("End"), None)
    if end is not None and end > start:
        return start, end

    frames = as_int(properties.get("Frames"), None)
    if frames is not None and frames > 0:
        return start, start + frames

    duration = str(properties.get("Duration") or "")
    if ":" in duration:
        length = frame_from_timecode(duration, fps)
    else:
        try:
            length = int(float(duration) * fps)
        except (TypeError, ValueError):
            length = 0
    if length <= 0:
        # 实在拿不到长度就按 5 秒算，至少能落进时间线让用户手动裁。
        length = int(fps * 5)
    return start, start + length


def track_is_free(timeline, index, start_frame, end_frame):
    """判断某条音轨在 [start_frame, end_frame) 区间内是否没有内容。"""
    try:
        items = timeline.GetItemListInTrack("audio", index) or []
    except Exception:  # noqa: BLE001
        return False
    for item in items:
        try:
            item_start = item.GetStart()
            item_end = item.GetEnd()
        except Exception:  # noqa: BLE001
            continue
        if item_start < end_frame and item_end > start_frame:
            return False
    return True


def pick_target_track(timeline, start_frame, end_frame):
    """挑一条不覆盖已有内容的音轨。

    优先复用播放头处本来就空着的音轨，全被占用时才新建一条，
    这样反复插入不会把音轨堆成一长串。
    """
    count = api_call(timeline, "GetTrackCount", "audio") or 0
    for index in range(1, count + 1):
        if track_is_free(timeline, index, start_frame, end_frame):
            return index, False

    try:
        if timeline.AddTrack("audio"):
            added = timeline.GetTrackCount("audio") or (count + 1)
            return added, True
    except Exception:  # noqa: BLE001 - 老版本没有 AddTrack 时退回第一轨
        pass
    return max(1, count), False


def main():
    if len(sys.argv) < 2:
        print("没有拿到音频文件路径")
        return 2

    audio_path = sys.argv[1]
    if not os.path.isfile(audio_path):
        print("音频文件不存在：" + audio_path)
        return 2

    # 波形区框选过一段时，会多传两个参数：选区起点和终点（秒）。
    range_start = None
    range_end = None
    if len(sys.argv) >= 4:
        try:
            range_start = float(sys.argv[2])
            range_end = float(sys.argv[3])
        except ValueError:
            range_start = None
            range_end = None
        if range_start is not None and range_end is not None and range_end <= range_start:
            range_start = None
            range_end = None

    problem = configure_resolve_environment()
    if problem:
        print(problem)
        return 1

    try:
        import DaVinciResolveScript as dvr_script
    except Exception as error:  # noqa: BLE001 - 需要把原始信息带回界面
        print("加载达芬奇脚本模块失败：" + str(error))
        return 1

    scriptapp = getattr(dvr_script, "scriptapp", None)
    if not callable(scriptapp):
        print("达芬奇脚本模块里没有 scriptapp，脚本库可能和达芬奇版本不匹配")
        return 1

    resolve = scriptapp("Resolve")
    if not resolve:
        print("没有连上 DaVinci Resolve，请确认达芬奇已经打开")
        return 1

    project_manager = api_call(resolve, "GetProjectManager")
    project = api_call(project_manager, "GetCurrentProject") if project_manager else None
    if not project:
        print("达芬奇里没有打开的项目")
        return 1

    timeline = api_call(project, "GetCurrentTimeline")
    if not timeline:
        print("当前项目没有激活的时间线")
        return 1

    media_pool = api_call(project, "GetMediaPool")
    if not media_pool:
        print("拿不到达芬奇的媒体池")
        return 1

    try:
        fps = float(api_call(timeline, "GetSetting", "timelineFrameRate") or 24)
    except (BridgeError, TypeError, ValueError):
        fps = 24.0

    playhead = None
    try:
        playhead = api_call(timeline, "GetCurrentFrame")
    except BridgeError:
        playhead = None
    if not isinstance(playhead, int) or playhead < 0:
        playhead = frame_from_timecode(api_call(timeline, "GetCurrentTimecode"), fps)

    imported = api_call(media_pool, "ImportMedia", [audio_path])
    item = imported[0] if imported else find_existing_clip(media_pool, audio_path)
    if item is None:
        print("导入媒体池失败，达芬奇可能不支持这个音频格式")
        return 1

    start, end = clip_span(item, fps)
    length = max(1, end - start)

    # 有选区就只把这一段放进时间线。startFrame/endFrame 是源素材的入出点，
    # 达芬奇自己按帧裁切，不需要另外生成临时 WAV。
    trimmed = range_start is not None and range_end is not None
    if trimmed:
        clip_start = start + int(round(range_start * fps))
        clip_end = start + int(round(range_end * fps))
        clip_start = max(start, min(clip_start, end - 1))
        clip_end = max(clip_start + 1, min(clip_end, end))
        start, end = clip_start, clip_end
        length = max(1, end - start)

    track_index, created = pick_target_track(timeline, playhead, playhead + length)

    placed = api_call(media_pool, "AppendToTimeline", [{
        "mediaPoolItem": item,
        "startFrame": start,
        "endFrame": end,
        "trackIndex": track_index,
        "recordFrame": playhead,
        "mediaType": 2,
    }])
    if not placed:
        print("达芬奇拒绝把这段音频放进时间线")
        return 1

    name = api_call(item, "GetName") or os.path.basename(audio_path)
    if trimmed:
        name = "%s（%.3f–%.3f 秒）" % (name, range_start, range_end)
    landed = None
    try:
        landed = placed[0].GetStart()
    except Exception:  # noqa: BLE001
        pass

    if isinstance(landed, int) and landed != playhead:
        print("已把「%s」追加到时间线，但落点是第 %d 帧而不是播放头 %d 帧；"
              "当前达芬奇版本可能不支持指定插入位置" % (name, landed, playhead))
        return 0

    if created:
        print("已插入「%s」到第 %d 轨（新建音轨）" % (name, track_index))
    else:
        print("已插入「%s」到第 %d 轨播放头" % (name, track_index))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BridgeError as error:
        print(str(error))
        sys.exit(1)
    except Exception as error:  # noqa: BLE001 - 把出错位置带回界面
        tb = sys.exc_info()[2]
        while tb is not None and tb.tb_next is not None:
            tb = tb.tb_next
        line = tb.tb_lineno if tb is not None else 0
        print("桥接脚本第 %d 行出错：%s: %s" % (line, type(error).__name__, error))
        sys.exit(1)

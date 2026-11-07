use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use cd_files::{
    copy_tree, partial_path, plan_conflicts, sanitize_component, sanitize_rel_path, scan_dir,
    ConflictAction, ConflictPolicy, Manifest, TargetPlatform, TransferState, TransferTask,
};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct FilesArgs {
    #[command(subcommand)]
    action: FilesAction,
}

#[derive(Debug, Subcommand)]
enum FilesAction {
    /// 目录 manifest、跨平台路径安全、冲突策略与续传状态
    SelfTest,
    /// 真实临时目录扫描、落盘复制与 .part 断点续跑
    #[command(name = "roundtrip", alias = "round-trip")]
    RoundTrip,
}

pub fn run(args: FilesArgs) -> ExitCode {
    match args.action {
        FilesAction::SelfTest => match self_test() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("文件/目录自检失败: {error}");
                ExitCode::FAILURE
            }
        },
        FilesAction::RoundTrip => match roundtrip() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("文件落盘往返失败: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn self_test() -> Result<(), String> {
    println!("文件/目录管道自检");

    let mut manifest = Manifest::new("设计素材").map_err(|error| error.to_string())?;
    manifest
        .push_dir("images", Some(1))
        .map_err(|error| error.to_string())?;
    manifest
        .push_file("images/logo.png", 2048, Some(2))
        .map_err(|error| error.to_string())?;
    manifest
        .push_dir("空目录", None)
        .map_err(|error| error.to_string())?;
    manifest
        .push_file("README.txt", 12, None)
        .map_err(|error| error.to_string())?;

    for target in [TargetPlatform::Linux, TargetPlatform::Windows] {
        let issues = manifest.validate(target);
        if !issues.is_empty() {
            return Err(format!("{target:?} manifest 校验失败: {issues:?}"));
        }
    }
    println!(
        "  manifest: {} 项 · {} 文件 · {} 目录（含空目录）· {} B",
        manifest.entries.len(),
        manifest.file_count(),
        manifest.directory_count(),
        manifest.total_bytes()
    );
    println!("  跨平台校验: Linux / Windows 均通过");

    if sanitize_rel_path("../escape", TargetPlatform::Windows).is_ok() {
        return Err("路径穿越未被拒绝".to_owned());
    }
    let reserved = sanitize_component("CON.txt", TargetPlatform::Windows);
    if reserved != "_CON.txt" {
        return Err(format!("Windows 保留名处理错误: {reserved}"));
    }
    println!("  路径安全: `../escape` 拒绝 · `CON.txt` → `_CON.txt`");

    let merge = plan_conflicts(
        &manifest,
        &["images/logo.png".to_owned()],
        &["images".to_owned()],
        ConflictPolicy::Merge,
        TargetPlatform::Linux,
    );
    if merge.count(ConflictAction::MergeDirectory) != 1 || merge.count(ConflictAction::Ask) != 1 {
        return Err("合并策略结果不符合预期".to_owned());
    }

    let rename = plan_conflicts(
        &manifest,
        &[],
        &["images".to_owned()],
        ConflictPolicy::Rename,
        TargetPlatform::Linux,
    );
    let renamed_dir = rename
        .entries
        .iter()
        .find(|entry| entry.rel_path == "images")
        .ok_or_else(|| "缺少 images 条目".to_owned())?;
    let renamed_child = rename
        .entries
        .iter()
        .find(|entry| entry.rel_path == "images/logo.png")
        .ok_or_else(|| "缺少 images/logo.png 条目".to_owned())?;
    if renamed_dir.destination != "images~2" || renamed_child.destination != "images~2/logo.png" {
        return Err(format!(
            "目录重命名未带动子项: {} / {}",
            renamed_dir.destination, renamed_child.destination
        ));
    }
    println!("  冲突策略: 目录合并 + 文件询问；目录重命名带动子项");

    let mut task = TransferTask::new(
        "demo-task",
        manifest.root_name.clone(),
        manifest.total_bytes(),
    );
    task.start().map_err(|error| error.to_string())?;
    task.progress(manifest.total_bytes() / 2)
        .map_err(|error| error.to_string())?;
    task.pause().map_err(|error| error.to_string())?;
    let paused = task.state;
    task.resume().map_err(|error| error.to_string())?;
    task.progress(manifest.total_bytes())
        .map_err(|error| error.to_string())?;
    task.complete().map_err(|error| error.to_string())?;
    if paused != TransferState::Paused || task.state != TransferState::Completed {
        return Err("传输任务状态机结果不符合预期".to_owned());
    }
    println!("  传输任务: queued → running → paused（断线不取消）→ running → completed");
    println!("[通过] 目录 manifest、路径安全、冲突与续传状态均可用");
    Ok(())
}

fn roundtrip() -> Result<(), String> {
    let base = temp_base();
    let result = roundtrip_inner(&base);
    let _ = fs::remove_dir_all(&base);
    result
}

fn roundtrip_inner(base: &Path) -> Result<(), String> {
    let src = base.join("source").join("素材");
    let dest = base.join("dest").join("素材");
    fs::create_dir_all(src.join("images")).map_err(|error| error.to_string())?;
    fs::create_dir_all(src.join("empty")).map_err(|error| error.to_string())?;
    fs::write(src.join("notes.txt"), "跨设备目录传输\n第二行\n".as_bytes())
        .map_err(|error| error.to_string())?;
    let payload = (0..32_768_u32)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<u8>>();
    fs::write(src.join("images/payload.bin"), &payload).map_err(|error| error.to_string())?;

    let manifest = scan_dir(&src).map_err(|error| error.to_string())?;
    let issues = manifest.validate(TargetPlatform::Linux);
    if !issues.is_empty() {
        return Err(format!("manifest 校验失败: {issues:?}"));
    }
    println!("文件落盘往返");
    println!(
        "  源目录: {} 文件 · {} 目录（含空目录）· {} B",
        manifest.file_count(),
        manifest.directory_count(),
        manifest.total_bytes()
    );

    let report = copy_tree(
        &src,
        &dest,
        ConflictPolicy::Overwrite,
        TargetPlatform::Linux,
    )
    .map_err(|error| error.to_string())?;
    assert_tree(&src, &dest, &report)?;
    println!(
        "  首次复制: {} 文件 · {} 目录 · {} B",
        report.copied_files, report.created_dirs, report.bytes_written
    );

    let dest2 = base.join("dest2").join("素材");
    fs::create_dir_all(&dest2).map_err(|error| error.to_string())?;
    let notes = fs::read(src.join("notes.txt")).map_err(|error| error.to_string())?;
    let part = partial_path(&dest2.join("notes.txt"));
    fs::write(&part, &notes[..2]).map_err(|error| error.to_string())?;
    let resumed = copy_tree(
        &src,
        &dest2,
        ConflictPolicy::Overwrite,
        TargetPlatform::Linux,
    )
    .map_err(|error| error.to_string())?;
    if resumed.resumed_files == 0 {
        return Err("预置 .cdpart 后未触发续跑".to_owned());
    }
    assert_tree(&src, &dest2, &resumed)?;
    println!(
        "  断点续跑: 预置 2 B 的 .cdpart，续传 {} 个文件 · 写入 {} B",
        resumed.resumed_files, resumed.bytes_written
    );

    let renamed = copy_tree(&src, &dest, ConflictPolicy::Rename, TargetPlatform::Linux)
        .map_err(|error| error.to_string())?;
    let renamed_note = dest.join("notes~2.txt");
    let renamed_payload = dest.join("images~2/payload.bin");
    if !renamed_note.is_file() || !renamed_payload.is_file() {
        return Err(format!(
            "冲突重命名未按目录树落盘: {} / {}",
            renamed_note.display(),
            renamed_payload.display()
        ));
    }
    println!(
        "  冲突重命名: notes~2.txt · images~2/payload.bin · {} 文件",
        renamed.copied_files
    );
    println!("[通过] 真实目录扫描、落盘复制与断点续跑均可用");
    Ok(())
}

fn assert_tree(src: &Path, dest: &Path, report: &cd_files::CopyReport) -> Result<(), String> {
    if report.task.state != TransferState::Completed {
        return Err("复制任务未完成".to_owned());
    }
    let notes = fs::read(src.join("notes.txt")).map_err(|error| error.to_string())?;
    if fs::read(dest.join("notes.txt")).map_err(|error| error.to_string())? != notes {
        return Err("notes.txt 内容不一致".to_owned());
    }
    let payload = fs::read(src.join("images/payload.bin")).map_err(|error| error.to_string())?;
    if fs::read(dest.join("images/payload.bin")).map_err(|error| error.to_string())? != payload {
        return Err("payload.bin 内容不一致".to_owned());
    }
    if !dest.join("empty").is_dir() {
        return Err("空目录未重建".to_owned());
    }
    Ok(())
}

fn temp_base() -> PathBuf {
    std::env::temp_dir().join(format!(
        "crossdev-files-roundtrip-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ))
}

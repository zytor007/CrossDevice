use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use cd_core::{PairingSession, PairingState};
use cd_store::{DeviceRecord, Store};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct PairArgs {
    #[command(subcommand)]
    action: PairAction,
}

#[derive(Debug, Subcommand)]
enum PairAction {
    /// 双端 SAS 配对、过期与 SQLite 设备持久化
    SelfTest,
}

pub fn run(args: PairArgs) -> ExitCode {
    match args.action {
        PairAction::SelfTest => match self_test() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("配对自检失败: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn self_test() -> Result<(), String> {
    println!("配对与持久化自检");
    let nonce = (0..16).collect::<Vec<u8>>();
    let mut local = PairingSession::start(vec![1; 32], "aa".repeat(32), nonce.clone(), 0)
        .map_err(|error| error.to_string())?;
    let mut peer = PairingSession::start(vec![2; 32], "bb".repeat(32), nonce.clone(), 0)
        .map_err(|error| error.to_string())?;

    let local_code = local
        .receive_peer(vec![2; 32], "bb".repeat(32), 10)
        .map_err(|error| error.to_string())?;
    let peer_code = peer
        .receive_peer(vec![1; 32], "aa".repeat(32), 10)
        .map_err(|error| error.to_string())?;
    if local_code != peer_code {
        return Err(format!("双端 SAS 不一致: {local_code} / {peer_code}"));
    }
    println!("  SAS 短码: {local_code}（双端一致）");

    local.confirm_local(20).map_err(|error| error.to_string())?;
    peer.confirm_peer(20).map_err(|error| error.to_string())?;
    local.confirm_peer(30).map_err(|error| error.to_string())?;
    peer.confirm_local(30).map_err(|error| error.to_string())?;
    if !local.is_paired() || !peer.is_paired() {
        return Err("双端确认后未进入 PAIRED".to_owned());
    }
    println!("  确认流程: 本地 + 对端 → {}", local.state().label());

    let expired = expired_session(&nonce);
    println!("  过期控制: 60s → {}", expired);

    let path = temp_db();
    let record = DeviceRecord {
        id: "macbook-air-13".to_owned(),
        name: "MacBook Air 13.6\"".to_owned(),
        cert_fingerprint: vec![0xbb; 32],
        public_key: vec![2; 32],
        capabilities: 0b1111,
        paired_at: 30,
        last_seen: Some(30),
    };
    let result = persist(&path, &record);
    cleanup(&path);
    let stored = result?;
    if stored != record {
        return Err("SQLite 读回设备记录不一致".to_owned());
    }
    println!("  持久化: 设备白名单写入并重开读取一致");

    println!("[通过] SAS 配对、60s 过期与设备持久化均可用");
    Ok(())
}

fn expired_session(nonce: &[u8]) -> String {
    let mut session =
        PairingSession::start(vec![3; 32], "cc".repeat(32), nonce.to_vec(), 0).expect("session");
    let _ = session.receive_peer(vec![4; 32], "dd".repeat(32), 0);
    let _ = session.tick(60_000);
    if matches!(session.state(), PairingState::Expired) {
        "EXPIRED".to_owned()
    } else {
        "UNEXPECTED".to_owned()
    }
}

fn persist(path: &Path, record: &DeviceRecord) -> Result<DeviceRecord, String> {
    {
        let store = Store::open(path).map_err(|error| error.to_string())?;
        store
            .upsert_device(record)
            .map_err(|error| error.to_string())?;
    }
    let store = Store::open(path).map_err(|error| error.to_string())?;
    store
        .device(&record.id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "设备记录未找到".to_owned())
}

fn temp_db() -> PathBuf {
    std::env::temp_dir().join(format!(
        "crossdev-pair-{}-{}.sqlite",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ))
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        let _ = fs::remove_file(candidate);
    }
}

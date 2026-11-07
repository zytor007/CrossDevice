use cd_core::{OwnershipAction, OwnershipEvent, Peer, Session};
use cd_geometry::{presets, Layout, Side};
use cd_proto::Modifiers;

use crate::profile::AirChoice;
use crate::report;

pub fn run_demo(choice: AirChoice, gap_mm: f64) -> bool {
    println!("双节点模拟 · linux-4k ↔ MacBook Air");
    println!("说明: 只模拟布局、穿越与输入所有权；不触碰真实输入设备。\n");

    let mut total = Tally::default();
    for air in choice.models() {
        let layout = presets::compose_air(air.clone(), gap_mm);
        println!("━━━ {} ━━━", air.name);
        run_profile(&air.id, &layout, &mut total);
        println!();
    }

    println!("总结果: {} 通过 / {} 失败", total.passed, total.failed);
    total.failed == 0
}

fn run_profile(air_id: &str, layout: &Layout, tally: &mut Tally) {
    let Some(air) = layout.screen(air_id) else {
        tally.check(false, "屏幕档案", format!("布局中缺少 {air_id}"));
        return;
    };
    let Some(local) = layout.screen(presets::LINUX_4K_ID) else {
        tally.check(false, "屏幕档案", "布局中缺少本机屏幕");
        return;
    };
    for check in report::checks_for(local, air, layout) {
        tally.check(check.passed, &check.name, &check.detail);
    }

    if let Some(pair) = layout.pair_edges().first() {
        let start = pair.overlap_start_mm;
        let end = pair.overlap_end_mm;
        println!(
            "  共享边 {:.1}–{:.1} mm（重叠 {:.1} mm）",
            start,
            end,
            pair.overlap_mm()
        );

        for fraction in [0.0, 0.5, 1.0] {
            let y = start + (end - start) * fraction;
            if let Some(check) = layout.check_roundtrip(presets::LINUX_4K_ID, Side::Right, y) {
                println!(
                    "    抽样 y={y:.1} mm → ({} {:.1}, {:.1}) px · t={:.3} · 归一化 {} · 回程 {:.2} px",
                    check.outbound.target_id,
                    check.outbound.target_logical.x,
                    check.outbound.target_logical.y,
                    check.outbound.overlap_t,
                    check.outbound.normalized_along,
                    check.error_px
                );
            } else {
                tally.check(false, "穿越抽样", format!("y={y:.1} mm 无法穿越"));
            }
        }

        if start > 10.0 {
            let dead = layout.find_crossing(presets::LINUX_4K_ID, Side::Right, start - 10.0);
            tally.check(
                dead.is_none(),
                "重叠段上方死区",
                format!("y={:.1} mm", start - 10.0),
            );
        }
        if end + 10.0 < 360.0 {
            let dead = layout.find_crossing(presets::LINUX_4K_ID, Side::Right, end + 10.0);
            tally.check(
                dead.is_none(),
                "重叠段下方死区",
                format!("y={:.1} mm", end + 10.0),
            );
        }
    } else {
        tally.check(false, "共享边", "未生成共享边，无法模拟穿越");
        return;
    }

    simulate_ownership(air_id, layout, tally);
}

fn simulate_ownership(air_id: &str, layout: &Layout, tally: &mut Tally) {
    println!("  所有权模拟");

    let mut session = new_session(air_id, layout);
    let actions = session.handle_edge_crossing(air_id, Modifiers::CTRL, 0);
    tally.check(
        session.ownership().state().label() == "HANDOFF_OUT"
            && has(&actions, |action| {
                matches!(action, OwnershipAction::HideLocalCursor)
            }),
        "越边进入移交",
        format!("状态 {}，隐藏本机光标", session.ownership().state().label()),
    );

    session.handle(OwnershipEvent::HandoffAccepted { at_ms: 10 });
    tally.check(
        session.ownership().state().label() == "REMOTE_OWNED",
        "对端确认接管",
        session.ownership().state().label(),
    );

    let actions = session.handle(OwnershipEvent::ReturnHome { at_ms: 500 });
    tally.check(
        session.ownership().state().label() == "HANDOFF_BACK"
            && has(&actions, |action| {
                matches!(action, OwnershipAction::ShowLocalCursor)
            }),
        "快捷键回切",
        "显示本机光标并通知对端释放",
    );

    session.handle(OwnershipEvent::Tick { at_ms: 800 });
    tally.check(
        session.ownership().is_local(),
        "回切收敛",
        format!("状态 {}", session.ownership().state().label()),
    );

    let mut slow = new_session(air_id, layout);
    slow.handle_edge_crossing(air_id, Modifiers::empty(), 0);
    let actions = slow.handle(OwnershipEvent::Tick { at_ms: 300 });
    tally.check(
        has(&actions, |action| {
            matches!(action, OwnershipAction::NotifyHandoffSlow { .. })
        }),
        "250ms 缓慢告警",
        "移交超时前提示",
    );
    let actions = slow.handle(OwnershipEvent::Tick { at_ms: 800 });
    tally.check(
        slow.ownership().is_local()
            && has(&actions, |action| {
                matches!(action, OwnershipAction::NotifyHandoffTimeout { .. })
            }),
        "750ms 超时回切",
        "自动收回本机所有权",
    );

    let mut priority = new_session(air_id, layout);
    priority.handle_edge_crossing(air_id, Modifiers::empty(), 0);
    priority.handle(OwnershipEvent::HandoffAccepted { at_ms: 10 });
    let actions = priority.handle(OwnershipEvent::LocalInputWhileRemote { at_ms: 100 });
    tally.check(
        priority.ownership().state().label() == "HANDOFF_BACK"
            && has(&actions, |action| {
                matches!(action, OwnershipAction::ShowLocalCursor)
            }),
        "本机输入优先",
        "远端接管期间本机动鼠标立即收回",
    );

    let mut lost = new_session(air_id, layout);
    lost.handle_edge_crossing(air_id, Modifiers::empty(), 0);
    lost.handle(OwnershipEvent::HandoffAccepted { at_ms: 10 });
    let actions = lost.set_peer_online(air_id, false, 500);
    tally.check(
        lost.ownership().is_local()
            && has(&actions, |action| {
                matches!(action, OwnershipAction::ShowLocalCursor)
            }),
        "对端掉线回切",
        "无需等待 watchdog",
    );
}

fn new_session(air_id: &str, layout: &Layout) -> Session {
    let mut session = Session::new(presets::LINUX_4K_ID, layout.clone());
    session.add_peer(Peer::new(air_id, "MacBook Air").with_online(true));
    session
}

fn has(actions: &[OwnershipAction], predicate: impl Fn(&OwnershipAction) -> bool) -> bool {
    actions.iter().any(predicate)
}

#[derive(Default)]
struct Tally {
    passed: usize,
    failed: usize,
}

impl Tally {
    fn check(&mut self, ok: bool, name: &str, detail: impl AsRef<str>) {
        if ok {
            self.passed += 1;
            println!("    [通过] {name} · {}", detail.as_ref());
        } else {
            self.failed += 1;
            println!("    [失败] {name} · {}", detail.as_ref());
        }
    }
}

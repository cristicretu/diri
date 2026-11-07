use std::path::Path;
use std::time::Duration;

use diri_client::NodeClient;
use diri_proto::paths::DirijorPaths;
use diri_proto::{HostsConfig, ProviderKind, UsageQueryParams};
use tokio::task::JoinSet;

use super::{Clock, ProviderUsage, SystemClock, UsageSnapshot, UsageTotals};

/// Merge every enrolled execution node into the local display projection.
/// Unreachable or legacy SSH-only hosts are intentionally best-effort: local
/// accounting remains available and the next reconciliation retries them.
pub async fn merge_fleet_usage(mut snapshot: UsageSnapshot, home: &Path) -> UsageSnapshot {
    let hosts = HostsConfig::load(DirijorPaths::hosts_config_file(home));
    let reading = SystemClock.read();
    let mut tasks = JoinSet::new();
    for host in hosts.hosts.into_iter().filter(|host| host.node.is_some()) {
        let home = home.to_owned();
        tasks.spawn(async move {
            tokio::time::timeout(Duration::from_secs(4), async {
                let client = NodeClient::from_host(&host, &home).ok()?;
                let today = client
                    .usage(UsageQueryParams {
                        from: Some(reading.today_started_at),
                        ..UsageQueryParams::default()
                    })
                    .await
                    .ok()?;
                let month = client
                    .usage(UsageQueryParams {
                        from: Some(reading.month_started_at),
                        ..UsageQueryParams::default()
                    })
                    .await
                    .ok()?;
                Some((today, month))
            })
            .await
            .ok()
            .flatten()
        });
    }
    // One unreachable host must not drop the hosts that answer after it.
    while let Some(joined) = tasks.join_next().await {
        if let Ok(Some((today, month))) = joined {
            merge_window(&mut snapshot.claude, &today, &month, ProviderKind::Claude);
            merge_window(&mut snapshot.codex, &today, &month, ProviderKind::Codex);
        }
    }
    snapshot
}

fn merge_window(
    destination: &mut ProviderUsage,
    today: &diri_proto::UsageQueryResult,
    month: &diri_proto::UsageQueryResult,
    provider: ProviderKind,
) {
    if let Some(totals) = today.by_provider.get(&provider) {
        destination.today += convert(totals, today.authoritative_billing_available);
    }
    if let Some(totals) = month.by_provider.get(&provider) {
        destination.month += convert(totals, month.authoritative_billing_available);
    }
}

fn convert(totals: &diri_proto::UsageTotals, authoritative: bool) -> UsageTotals {
    UsageTotals {
        input_tokens: totals.input_tokens,
        output_tokens: totals.output_tokens,
        cache_read_tokens: totals.cache_read_tokens,
        cache_write_tokens: totals.cache_write_tokens,
        cost: if authoritative {
            totals.billed_usd
        } else {
            totals.estimated_usd
        },
    }
}

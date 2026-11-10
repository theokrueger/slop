use std::{collections::HashSet, time::Duration};

use futures_util::future::join_all;
use sysinfo::{Disks, System};
use tokio::process::Command;

use crate::config::{Service, StatusConfig};

#[derive(Debug, Clone, Default)]
pub struct SystemStatus {
    pub storage: Vec<StorageInfo>,
    pub ram: Option<RamInfo>,
    pub uptime: Option<u64>,
    pub services: Vec<ServiceStatus>,
}

#[derive(Debug, Clone)]
pub struct StorageInfo {
    pub mount: String,
    pub fs: String,
    pub total: u64,
    pub used: u64,
}

#[derive(Debug, Clone)]
pub struct RamInfo {
    pub used: u64,
    pub total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
}

#[derive(Debug, Clone)]
pub struct ServiceStatus {
    pub name: String,
    pub user: bool,
    pub state: String,
    pub sub_state: String,
    pub description: String,
    pub error: Option<String>,
}

pub async fn collect(config: &StatusConfig) -> SystemStatus {
    let mut status = SystemStatus::default();

    if config.storage {
        status.storage = storage();
    }

    if config.ram {
        let mut system = System::new();
        system.refresh_memory();
        status.ram = Some(RamInfo {
            used: system.used_memory(),
            total: system.total_memory(),
            swap_used: system.used_swap(),
            swap_total: system.total_swap(),
        });
    }

    if config.uptime {
        status.uptime = Some(System::uptime());
    }

    if !config.status.is_empty() {
        status.services = join_all(config.status.iter().cloned().map(service_status)).await;
    }

    status
}

fn storage() -> Vec<StorageInfo> {
    let disks = Disks::new_with_refreshed_list();
    let mut seen = HashSet::new();
    let mut storage: Vec<_> = disks
        .list()
        .iter()
        .filter(|disk| disk.total_space() > 0)
        .filter(|disk| !is_virtual(disk.file_system().to_string_lossy().as_ref()))
        .filter(|disk| seen.insert(disk.mount_point().to_path_buf()))
        .map(|disk| {
            let total = disk.total_space();
            StorageInfo {
                mount: disk.mount_point().display().to_string(),
                fs: disk.file_system().to_string_lossy().into_owned(),
                total,
                used: total.saturating_sub(disk.available_space()),
            }
        })
        .collect();
    storage.sort_by(|a, b| a.mount.cmp(&b.mount));
    storage
}

fn is_virtual(fs: &str) -> bool {
    matches!(
        fs,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "cgroup"
            | "cgroup2"
            | "pstore"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "configfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "binfmt_misc"
            | "autofs"
            | "rpc_pipefs"
            | "nsfs"
            | "tmpfs"
    )
}

async fn service_status(service: Service) -> ServiceStatus {
    let name = service.name().to_owned();
    let user = service.user();
    let mut status = ServiceStatus {
        name,
        user,
        state: "unknown".into(),
        sub_state: String::new(),
        description: String::new(),
        error: None,
    };

    let mut command = Command::new("systemctl");
    if user {
        command.arg("--user");
    }
    command
        .args([
            "show",
            "-p",
            "ActiveState",
            "-p",
            "SubState",
            "-p",
            "Description",
            "--value",
            &status.name,
        ])
        .kill_on_drop(true);

    match tokio::time::timeout(Duration::from_secs(2), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let (state, sub_state, description) =
                parse_show(&String::from_utf8_lossy(&output.stdout));
            status.state = state;
            status.sub_state = sub_state;
            status.description = description;
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            status.error = Some(if stderr.trim().is_empty() {
                format!("systemctl exited with {}", output.status)
            } else {
                stderr.trim().to_owned()
            });
        }
        Ok(Err(error)) => status.error = Some(error.to_string()),
        Err(_) => status.error = Some("systemctl timed out".into()),
    }

    status
}

fn parse_show(stdout: &str) -> (String, String, String) {
    let mut lines = stdout.lines();
    let state = lines.next().unwrap_or_default().trim();
    let sub_state = lines.next().unwrap_or_default().trim();
    let description = lines.next().unwrap_or_default().trim();
    (
        if state.is_empty() { "unknown" } else { state }.to_owned(),
        sub_state.to_owned(),
        description.to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_systemctl_show_output() {
        let (state, sub, description) = parse_show("active\nrunning\nMy service\n");
        assert_eq!(state, "active");
        assert_eq!(sub, "running");
        assert_eq!(description, "My service");
    }

    #[test]
    fn handles_empty_systemctl_output() {
        let (state, sub, description) = parse_show("");
        assert_eq!(state, "unknown");
        assert!(sub.is_empty());
        assert!(description.is_empty());
    }

    #[test]
    fn filters_virtual_filesystems() {
        assert!(is_virtual("proc"));
        assert!(is_virtual("tmpfs"));
        assert!(!is_virtual("ext4"));
        assert!(!is_virtual("btrfs"));
    }

    #[tokio::test]
    async fn disabled_status_collects_nothing() {
        let config = StatusConfig {
            storage: false,
            ram: false,
            uptime: false,
            network: false,
            status: Vec::new(),
        };
        let status = collect(&config).await;
        assert!(status.storage.is_empty());
        assert!(status.ram.is_none());
        assert!(status.uptime.is_none());
        assert!(status.services.is_empty());
    }
}

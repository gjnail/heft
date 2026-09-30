//! WSL and Docker virtual disks. A WSL 2 distro or Docker Desktop keeps its
//! Linux file system in a `.vhdx` file that grows as Linux writes data but
//! never shrinks when that data is deleted. Compacting hands the unused space
//! back to Windows.
//!
//! Steps: optionally `fstrim` inside each WSL distro (so the disk knows which
//! blocks are free), `wsl --shutdown` to release the files, then diskpart's
//! `compact vdisk` in one elevated run (one UAC prompt for all disks).

use std::collections::HashSet;
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::reg::{Hive, Key};
use crate::winsys;

const LXSS: &str = r"Software\Microsoft\Windows\CurrentVersion\Lxss";
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;

#[derive(Clone, Debug, PartialEq)]
pub struct VDisk {
    /// Distro name, or "Docker Desktop data".
    pub name: String,
    pub path: PathBuf,
    /// Bytes the file takes on disk.
    pub size: u64,
    /// A WSL distro Heft can `fstrim` before compacting.
    pub distro: Option<String>,
    pub docker: bool,
    /// Sparse disks (WSL's `--set-sparse`) shrink by themselves.
    pub sparse: bool,
}

fn on_disk(path: &Path) -> Option<(u64, bool)> {
    let md = std::fs::metadata(path).ok()?;
    let sparse = md.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0;
    let size = if sparse { crate::platform::compressed_size(path).unwrap_or(md.len()) } else { md.len() };
    Some((size, sparse))
}

/// Every WSL 2 and Docker Desktop disk on this PC.
pub fn find() -> Vec<VDisk> {
    let docker: Vec<PathBuf> = [
        winsys::env_path("LOCALAPPDATA").map(|p| p.join(r"Docker\wsl\disk\docker_data.vhdx")),
        winsys::env_path("LOCALAPPDATA").map(|p| p.join(r"Docker\wsl\data\ext4.vhdx")),
        winsys::env_path("LOCALAPPDATA").map(|p| p.join(r"Docker\wsl\main\ext4.vhdx")),
        winsys::env_path("ProgramData").map(|p| p.join(r"DockerDesktop\vm-data\DockerDesktop.vhdx")),
    ]
    .into_iter()
    .flatten()
    .collect();
    find_in(LXSS, &docker)
}

/// `find`, with the registry key and Docker locations passed in (for tests).
fn find_in(lxss: &str, docker_paths: &[PathBuf]) -> Vec<VDisk> {
    let mut out: Vec<VDisk> = Vec::new();
    if let Some(root) = Key::open(Hive::CurrentUser, lxss) {
        for sub in root.subkeys() {
            let Some(k) = Key::open(Hive::CurrentUser, &format!(r"{lxss}\{sub}")) else { continue };
            let (Some(name), Some(base)) = (k.get_string("DistributionName"), k.get_string("BasePath")) else { continue };
            if k.get_dword("Version").is_some_and(|v| v < 2) {
                continue; // WSL 1 keeps files directly on NTFS.
            }
            let file = k.get_string("VhdFileName").unwrap_or_else(|| "ext4.vhdx".into());
            let path = PathBuf::from(base.trim_start_matches(r"\\?\")).join(file);
            let Some((size, sparse)) = on_disk(&path) else { continue };
            let docker = name.starts_with("docker-desktop");
            out.push(VDisk { distro: (!docker).then(|| name.clone()), name, path, size, docker, sparse });
        }
    }
    for path in docker_paths {
        if out.iter().any(|d| d.path.to_string_lossy().eq_ignore_ascii_case(&path.to_string_lossy())) {
            continue;
        }
        let Some((size, sparse)) = on_disk(path) else { continue };
        out.push(VDisk { name: "Docker Desktop data".into(), path: path.clone(), size, distro: None, docker: true, sparse });
    }
    out.sort_by_key(|d| std::cmp::Reverse(d.size));
    out
}

/// Docker Desktop has to be quit first: it restarts WSL behind our back.
pub fn docker_running(running: &HashSet<String>) -> bool {
    // Not com.docker.service: that service keeps running after the app quits.
    ["docker desktop.exe", "com.docker.backend.exe"].iter().any(|p| running.contains(*p))
}

/// diskpart script that compacts each disk.
fn diskpart_script(disks: &[&Path]) -> String {
    let mut s = String::new();
    for d in disks {
        s.push_str(&format!("select vdisk file=\"{}\"\r\n", d.display()));
        s.push_str("attach vdisk readonly\r\ncompact vdisk\r\ndetach vdisk\r\n");
    }
    s.push_str("exit\r\n");
    s
}

#[derive(Default)]
pub struct Progress {
    pub phase: Mutex<String>,
}

impl Progress {
    fn set(&self, s: impl Into<String>) {
        *self.phase.lock().unwrap() = s.into();
    }
}

#[derive(Debug)]
pub struct Compacted {
    pub name: String,
    pub before: u64,
    pub after: u64,
}

/// Whether nothing (WSL, Docker's VM) has the file open any more.
fn released(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().read(true).share_mode(0).open(path).is_ok()
}

fn wsl(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("wsl.exe")
        .args(args)
        .creation_flags(winsys::CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("wsl.exe: {e}"))
}

/// Trim (optionally), shut WSL down, and compact the disks. Blocks for as
/// long as diskpart takes, which can be minutes for large disks.
pub fn compact(disks: &[VDisk], trim: bool, p: &Progress) -> Result<Vec<Compacted>, String> {
    let disks: Vec<&VDisk> = disks.iter().filter(|d| !d.sparse).collect();
    if disks.is_empty() {
        return Ok(Vec::new());
    }
    if disks.iter().any(|d| d.docker) && docker_running(&winsys::running_processes()) {
        return Err("Quit Docker Desktop first.".into());
    }
    if trim {
        for d in &disks {
            if let Some(name) = &d.distro {
                p.set(format!("Trimming free space in {name}…"));
                // Best effort: an old kernel or a distro without fstrim still compacts.
                let _ = wsl(&["-d", name, "-u", "root", "fstrim", "-av"]);
            }
        }
    }
    p.set("Shutting down WSL…");
    wsl(&["--shutdown"])?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !disks.iter().all(|d| released(&d.path)) {
        if Instant::now() > deadline {
            return Err("A virtual disk is still in use. Close Docker Desktop and any WSL windows, then try again.".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let before: Vec<u64> = disks.iter().map(|d| on_disk(&d.path).map_or(d.size, |x| x.0)).collect();
    let tmp = std::env::temp_dir();
    let script = tmp.join(format!("heft-compact-{}.txt", std::process::id()));
    let log = tmp.join(format!("heft-compact-{}.log", std::process::id()));
    let paths: Vec<&Path> = disks.iter().map(|d| d.path.as_path()).collect();
    std::fs::write(&script, diskpart_script(&paths)).map_err(|e| e.to_string())?;

    p.set("Compacting (Windows asks for administrator permission)…");
    let args = format!("/c \"diskpart /s \"{}\" > \"{}\" 2>&1\"", script.display(), log.display());
    let result = winsys::launch_elevated_hidden("cmd.exe", &args);
    let code = match result {
        Ok(Some(proc)) => proc.wait(),
        Ok(None) => None,
        Err(e) => {
            let _ = std::fs::remove_file(&script);
            return Err(if e == "cancelled" { "Cancelled at the administrator prompt.".into() } else { e });
        }
    };
    let output = std::fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let _ = std::fs::remove_file(&script);
    let _ = std::fs::remove_file(&log);
    if code != Some(0) {
        let detail = output.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("diskpart failed");
        return Err(format!("diskpart: {detail}"));
    }
    Ok(disks
        .iter()
        .zip(before)
        .map(|(d, before)| Compacted { name: d.name.clone(), before, after: on_disk(&d.path).map_or(before, |x| x.0) })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script() {
        let s = diskpart_script(&[Path::new(r"C:\a b\ext4.vhdx"), Path::new(r"D:\docker_data.vhdx")]);
        assert_eq!(
            s,
            "select vdisk file=\"C:\\a b\\ext4.vhdx\"\r\nattach vdisk readonly\r\ncompact vdisk\r\ndetach vdisk\r\n\
             select vdisk file=\"D:\\docker_data.vhdx\"\r\nattach vdisk readonly\r\ncompact vdisk\r\ndetach vdisk\r\nexit\r\n"
        );
    }

    /// Discovery against a throwaway registry key and fake disk files.
    #[test]
    fn finds_distros_and_docker() {
        let dir = std::env::temp_dir().join(format!("heft-vdisk-{}", std::process::id()));
        for sub in ["ubuntu", "wsl1", "docker"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join(r"ubuntu\ext4.vhdx"), vec![0u8; 3000]).unwrap();
        std::fs::write(dir.join(r"docker\docker_data.vhdx"), vec![0u8; 5000]).unwrap();

        let lxss = format!(r"Software\HeftTest-{}\Lxss", std::process::id());
        let add = |guid: &str, name: &str, base: &str, version: u32| {
            let k = Key::create(Hive::CurrentUser, &format!(r"{lxss}\{guid}")).unwrap();
            let sz = |s: &str| s.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect::<Vec<u8>>();
            k.set_raw("DistributionName", crate::reg::REG_SZ, &sz(name)).unwrap();
            k.set_raw("BasePath", crate::reg::REG_SZ, &sz(base)).unwrap();
            k.set_raw("Version", crate::reg::REG_DWORD, &version.to_le_bytes()).unwrap();
        };
        add("{1}", "Ubuntu", &format!(r"\\?\{}", dir.join("ubuntu").display()), 2);
        add("{2}", "Old", &dir.join("wsl1").display().to_string(), 1);
        add("{3}", "Gone", &dir.join("missing").display().to_string(), 2);

        let found = find_in(&lxss, &[dir.join(r"docker\docker_data.vhdx"), dir.join(r"docker\nope.vhdx")]);
        let summary: Vec<(&str, u64, Option<&str>, bool)> =
            found.iter().map(|d| (d.name.as_str(), d.size, d.distro.as_deref(), d.docker)).collect();
        assert_eq!(summary, [("Docker Desktop data", 5000, None, true), ("Ubuntu", 3000, Some("Ubuntu"), false)]);

        crate::reg::delete_tree(Hive::CurrentUser, &format!(r"Software\HeftTest-{}", std::process::id())).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
